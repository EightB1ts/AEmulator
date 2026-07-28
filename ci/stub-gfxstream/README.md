# `ci/stub-gfxstream` — the gfxstream CI **link stub**

**This is not a renderer.** It draws nothing, it allocates nothing, and every entry point in it fails.
It exists for exactly one reason: to satisfy the **linker** in CI.

**It must never be on a developer's or a user's library path.** Nothing here belongs in a shipped
build, a release artifact, a run script, or `third_party/`. Do not set `GFXSTREAM_BUILD_HOST` to this
directory in any file a normal build reads (`third_party/paths.sh`, a run-\*.sh, Studio's `spawn.rs`,
a shell profile) — only in the GitHub job that needs it. See **"How this could go wrong"** below.

## Why it exists

`crates/aemulator-hvf` (~10k lines: the whole VMM and graphics stack) is in `[workspace].exclude`, so
the workspace-wide `cargo fmt` / `clippy` / `test` in `ci/check.sh` steps 1–3 **provably cannot reach
it**. `ci/check.sh` step 6 runs those three per-manifest, but it is gated on Apple-Silicon macOS *plus*
a built `libgfxstream_backend.dylib` — and `.github/workflows/aemulator-ci.yml`'s original job runs on
`ubuntu-latest`, where both conditions are false. So in GitHub CI the crate was skipped entirely: zero
automated coverage of the VMM, virtio devices, and graphics stack.

The blocker is `build.rs`, which links `-lgfxstream_backend` **unconditionally** on macOS. That is
deliberate: there is no cargo feature to build without the GPU path, because features were exactly how
this repo shipped a binary that *booted perfectly* and was quietly crippled (see the invariant comment
in `crates/aemulator-hvf/Cargo.toml`). Building the real renderer in CI means gfxstream **plus Mesa**:
~45 minutes on a hosted runner, i.e. a non-starter.

The way out is a property of the tests themselves: **every test in the crate is hermetic** — mock
renderer, mock guest mapper, in-process `TestQueue`. Not one of them *calls* into the dylib; they only
need the crate to **link**. A dylib that exports the same symbols as no-ops is therefore sufficient to
turn all of it into CI-covered code. Verified: 102 tests pass against this stub on a hosted-runner-class
macOS host, with no stub entry point ever reached.

## What is in here

| File | Role |
|---|---|
| `gfxstream_stub.c` | Every `extern "C"` function `crates/aemulator-hvf/src/gfxstream/ffi.rs` declares (53 of them as of 2026-07), as loud no-ops. |
| `build.sh` | Builds the stub **and** runs the symbol-parity guard. `--verify` = parity check only, into a temp dir. |
| `.gitignore` | The built `libgfxstream_backend.{dylib,so}` is an artifact; never commit it. |

`build.sh` prints the absolute output directory as the last line of stdout, so:

```bash
export GFXSTREAM_BUILD_HOST="$(bash ci/stub-gfxstream/build.sh --out "$RUNNER_TEMP/gfxstream-stub" | tail -1)"
cargo test --manifest-path crates/aemulator-hvf/Cargo.toml
```

macOS produces `libgfxstream_backend.dylib` (the requirement). Linux produces
`libgfxstream_backend.so` — free, since the same C file compiles with `-shared -fPIC`, but understand
what it is *for*: `build.rs` returns early on any non-macOS target and never links gfxstream at all, so
nothing on Linux consumes the `.so`. It exists so the **parity guard can run on the Linux gate**
(`ci/check.sh`, i.e. the `ubuntu-latest` job), catching stub drift before the macOS job even starts.

### It cannot be mistaken for the real renderer

A silently-*working* stub is the dangerous outcome, so the stub is loud in four independent ways:

1. A `__attribute__((constructor))` prints a banner at **dyld load time**, before `main`, whether or
   not anything calls in. Every test binary linked against it says so in the CI log.
2. Every entry point prints a line to stderr **naming itself** on first call, saying
   `THIS IS THE CI LINK STUB, NOT A RENDERER`, and telling you to build the real thing.
3. `stream_renderer_init()` returns **non-zero**. gfxstream's contract is `0 == success`, so the VMM's
   own error path refuses to proceed rather than running on a renderer that does not exist.
4. **No out-parameter is ever written.** No fabricated caps, no fabricated `GL_RENDERER` string, no
   host address handed back from `stream_renderer_resource_map`. Nothing downstream can read a
   plausible-looking value.

## The symbol-parity guard

The failure mode this guards is the stub going **stale**: `ffi.rs` grows a new `stream_renderer_*`
import, the stub does not, the macOS job stops linking, and the honest repair under time pressure is to
delete the job — putting the crate back at zero coverage.

So `build.sh` re-derives the symbol list from `ffi.rs` after every build and diffs it against what the
freshly-built stub exports, **failing with the missing names**:

```
      missing from the stub: android_redrawOpenglesWindow
      missing from the stub: stream_renderer_release_metal_layer
  ✗ 2 symbol(s) declared in
    crates/aemulator-hvf/src/gfxstream/ffi.rs are NOT exported by ci/stub-gfxstream (listed above).
    ...
```

It is the same idiom as `ci/hvf-doctor.sh`'s *"gfxstream ABI match"* step — `nm` for defined symbols,
strip the leading Mach-O underscore, `sort -u`, compare with `comm -23` — with the **declared** imports
standing in for a linked binary's `nm -u` output.

Two design notes:

- **Deriving from `ffi.rs` is strictly stronger than "does `cargo build` link?"** An `extern "C"`
  declaration that no Rust code calls *yet* produces no undefined symbol and no link error, so the
  plain build stays green and the stub only breaks on the later commit that first calls it. Checking
  the declarations catches it at the commit that adds them.
- **The guard is shell, not a Rust test in the crate**, for two reasons: `crates/aemulator-hvf/src/**`
  is under concurrent edit by other workstreams (a new test module there is a merge collision), and a
  shell step also runs on the Linux gate, where the crate cannot be built at all. `ci/check.sh` invokes
  `build.sh --verify` so the guard runs on every gate, on both platforms, local and hosted.
- There is also a **non-vacuity floor** (`MIN_SYMBOLS` in `build.sh`): if the extraction parses fewer
  than 40 symbols it fails rather than passing on an empty list. A vacuous parity check is worse than
  no parity check. If a bindgen bump changes the output shape, fix the `awk` — **do not lower the
  floor.**

## Regenerating the symbol list when `ffi.rs` changes

`ffi.rs` is generated (`crates/aemulator-hvf/src/gfxstream/gen-ffi.sh` → bindgen); the symbol set here
is derived from it and must never be hand-guessed. When it changes:

1. `bash ci/stub-gfxstream/build.sh --verify` — it names every symbol that is missing (and warns about
   any the stub still exports that `ffi.rs` no longer declares).
2. For each missing name, add a no-op to `gfxstream_stub.c`:
   - the **return type must be faithful** (`int` vs `void` vs a pointer) — that is the only part of the
     signature that matters at the ABI boundary, since the stub dereferences nothing;
   - parameters mirror `ffi.rs` for documentation value; struct pointers are spelled `void *`;
   - first statement is `AEMU_STUB(the_symbol_name);`;
   - return `AEMU_STUB_FAIL` (`-1`) if it returns an integer, `NULL` if it returns a pointer, nothing
     if it returns `void`;
   - **write no out-parameters**, and note in a comment when a caller might have expected you to.
3. Re-run `build.sh --verify` until it is green, then `bash ci/check.sh`.

To see the authoritative list yourself:

```bash
awk '/^[[:space:]]*(unsafe[[:space:]]+)?extern[[:space:]]+"C"[[:space:]]*\{/ {b=1;next}
     b && /^[[:space:]]*\}/ {b=0;next}
     b && /pub[[:space:]]+fn[[:space:]]/ {sub(/^[[:space:]]*pub[[:space:]]+fn[[:space:]]+/,"");sub(/[(<].*$/,"");print}' \
    crates/aemulator-hvf/src/gfxstream/ffi.rs | sort -u
```

Phase 4.6 of the graphics plan adds `stream_renderer_set_gpu_identity()`. That is exactly the case this
guard is for: the commit that lands it in `ffi.rs` must add the stub here in the same change.

## How this could go wrong (read before you `export`)

`build.rs` bakes `$GFXSTREAM_BUILD_HOST` in as an `-rpath`, so a binary linked against this stub will
keep resolving `libgfxstream_backend.dylib` **to the stub** at run time, from wherever it was when you
built. That is the whole reason the stub shouts on load: a `GFXSTREAM_BUILD_HOST` left exported in a
shell profile would produce a binary that builds green, launches, and has no graphics — the same shape
of failure as the crippled feature-flag build that got the cargo features deleted.

Countermeasures, in order of reliability:

- Point `GFXSTREAM_BUILD_HOST` at the stub only in a **process-scoped** export (the GitHub job, or one
  command line), never in a file. `build.rs` declares `cargo:rerun-if-env-changed=GFXSTREAM_BUILD_HOST`,
  so unsetting it correctly triggers a relink against the real dylib.
- Use a **separate `CARGO_TARGET_DIR`** when experimenting locally, so `crates/aemulator-hvf/target/`
  keeps the real build. `ci/hvf-doctor.sh` reads binaries out of that directory.
- If you suspect a stub-linked binary: `otool -l <bin> | grep -A2 LC_RPATH`, or just run it — the
  banner is the first thing on stderr.
- `ci/hvf-doctor.sh` remains the real-host gate (real driver, ad-hoc codesign, Vulkan ICD) and is
  deliberately **not** run in CI. A green CI run says the crate compiles, lints and passes its hermetic
  tests. It says **nothing** about whether the graphics stack works. Only `hvf-doctor.sh`,
  `kosmickrisp/run-gate.sh` and a real `run-android64.sh --gpu3d --display` boot say that.
