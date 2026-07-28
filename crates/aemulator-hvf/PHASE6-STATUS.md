# Phase 6 — LLM Control API (host-driven control plane)

**Goal:** turn the two load-bearing, *undetectable* host-side primitives we already had —
**framebuffer capture** (`src/display/mod.rs`, the guest scanout) and **virtio-input injection**
(`src/virtio/{input.rs,pci.rs}` + `src/machine.rs` inject thread) — into a clean, scriptable
**CONTROL INTERFACE** an LLM (or a later MCP server) can drive to *use* the Android guest. No adb,
no in-guest `AccessibilityService` — everything runs host-side against the guest's own pixels and
its virtio-input touchscreen/keyboard.

Status: **DONE + screenshot-verified.** A persistent control server (`aemulator-hvf control`)
serves a one-line text protocol over a Unix-domain socket (+ optional stdin REPL / script file); a
one-shot client (`aemulator-hvf tap/swipe/screenshot/…`) and the deferred MCP server both drive it
through the same seam. Proven end-to-end: from a booted 64-bit Android, host-injected swipes
dismissed the keyguard and opened the app drawer, captured as before/after PNGs straight from the
guest scanout. Default build stays hermetic; Phase-1 regression green (32/32 lib tests).

---

## What was built

### 1. The control plane — `src/control/`

- **`src/control/mod.rs`** — the command protocol, executor, and transports.
  - `run_control(bridge, opts)` owns the process **main thread** after boot (the VMM runs on a
    worker thread and hands back a `DisplayBridge`, exactly like the `--display` path). No window is
    opened — screenshots come straight from the guest scanout, so it works on a **headless/remote**
    session (same rationale as the existing `AEMU_FB_DUMP`).
  - `Controller` executes commands; all input/console senders live behind ONE mutex so a multi-frame
    gesture (a swipe, a typed string) is injected atomically — two concurrent clients can never
    interleave events mid-gesture.
  - Coordinates are **display space** (`1024×768`, `GPU_WIDTH×GPU_HEIGHT`); `map_abs` maps them into
    the virtio-input absolute range `0..=ABS_MAX` before injection.
- **`src/control/png.rs`** — a tiny, dependency-free PNG encoder (8-bit RGB) so `screenshot` returns
  a real PNG with **no new crate** (the default build must stay offline/hermetic). IDAT is a valid
  zlib stream built entirely from DEFLATE **stored** (uncompressed) blocks — no compressor needed;
  we implement the two checksums the containers require (CRC-32 for PNG chunks, Adler-32 for zlib).
  Output verified decodable by macOS `sips` + `file` (`PNG image data, … 8-bit/color RGB`).

### 2. Transports (all speak the SAME one-line protocol)

| Transport | How | Use |
|---|---|---|
| **Unix-domain socket** (default `/tmp/aemulator-hvf-control.sock`, `--socket <p>`) | `run_control` listens; clients connect, send one request line, read one response line | The persistent channel; the **seam a thin MCP server wraps** |
| **stdin REPL** (`--repl`) | request lines on stdin → responses on stdout | Interactive session in one terminal |
| **`--script <file>`** (`--exit-after-script`) | newline commands executed at startup | Scripted/batch runs |

Control mode sets `AEMU_CONTROL_NO_CONSOLE_STDIN=1` so stdin is free for the REPL; the guest serial
console is instead driven over the bridge's new `console_tx` (used by `launch`). This is env-gated so
the `--display` and headless paths stay byte-identical (Phase-1 regression untouched).

### 3. Command protocol

```
Request  = one line: <verb> [args…]
Response = one line: OK …   |   ERR …
```

| Verb | Meaning |
|---|---|
| `screenshot [path]` / `shot [path]` | encode the current guest scanout as PNG to `path` (default `/tmp/aemulator-shot-<ms>.png`); returns `OK <path> WxH gen=N nonzero=K bytes=B` |
| `tap X Y` | touch-down → hold → touch-up at display-space (X,Y) |
| `swipe X1 Y1 X2 Y2 [MS]` | interpolated directional drag over MS ms (default 300) |
| `text <string>` | inject as evdev key events (with SHIFT for uppercase / shifted symbols) |
| `key <code|name>` | one evdev key press+release (numeric code <256, or `ENTER/BACK/HOME/MENU/POWER/TAB/…`) |
| `keydown <code|name>` | press and HOLD — the caller owns the matching `keyup`. Required for held modifiers (`CMD`/`CTRL`/`ALT`/`SHIFT` → Cmd+A, Ctrl+C, shift-arrow) and for forwarding a live keyboard's own edges |
| `keyup <code|name>` | release a held key (harmless for a key that isn't down) |
| `launch <pkg>` | best-effort `am start`/`monkey` over the guest serial console (needs a console shell; input-driven nav is the primary path) |
| `wait_stable [TIMEOUT_MS] [INTERVAL_MS]` | poll the scanout content-hash until steady (3 consecutive equal ticks) or timeout |
| `hash` / `info` | scanout content-hash (FNV-1a over fb bytes) + flush gen + nonzero-byte count |
| `help` / `quit` | list verbs / stop the server |

### 4. CLI surface — `src/bin/aemulator-hvf.rs`

- `aemulator-hvf control [<run-android flags>] [--socket P] [--script F] [--repl] [--exit-after-script]`
  — boot Android + serve the control plane (equivalent to `run-android --control`).
- One-shot **clients**: `aemulator-hvf tap 500 400`, `… swipe 512 700 512 80 250`, `… text hello`,
  `… key ENTER`, `… screenshot /tmp/s.png`, `… wait_stable`, `… launch <pkg>`, `… info`, `… hash`
  — each connects to a running server's socket (`--socket` override), prints the response, and maps
  `ERR …` to a non-zero exit.

### 5. Harnesses

- **`run-control64.sh`** — identical image prep + build + ad-hoc codesign to `run-android64.sh`;
  only the final invocation changes to `run-android --control`. `SWRENDER=1` renders into the scanout
  (the proven home-screen path).
- **`prove-control64.sh`** — the end-to-end proof driver (see below).

---

## Proof (screenshot-verified)

`./prove-control64.sh` boots the 64-bit Android under the control server, then drives it **entirely
over the control socket** — no adb, no a11y:

1. Poll the scanout until the guest has rendered and the content-hash is stable.
2. `screenshot` → **`screenshots/phase6-control-01-initial.png`** (keyguard/lockscreen).
3. Inject a bottom→top `swipe 512 700 512 80` to dismiss the (insecure, swipe-only) keyguard →
   `wait_stable` → `screenshot` → **`screenshots/phase6-control-02-home.png`** (home screen).
4. Inject a hotseat `swipe 512 745 512 150` (app-drawer swipe-up) → `wait_stable` → `screenshot` →
   **`screenshots/phase6-control-03-appdrawer.png`** (a NEW screen).
5. `tap 160 250` → `screenshot` → **`screenshots/phase6-control-04-after-tap.png`**.

**Verification is the framebuffer content-hash CHANGING across the injected gestures** — the guest
scanout pixels moved *because* of host-driven input. See the run's `[prove] SUMMARY` block and the
before/after PNGs.

<!-- RESULT_PLACEHOLDER -->

---

## MCP-wrap seam (the deferred MCP server)

The Unix-domain socket + one-line protocol IS the seam. A thin MCP server does **no VMM work** — it
opens the socket and translates MCP tool calls to protocol lines:

```
MCP tool `screenshot()`      → write "screenshot /tmp/x.png\n" → read "OK …" → return the PNG bytes
MCP tool `tap(x, y)`         → write "tap {x} {y}\n"           → read "OK …"
MCP tool `swipe(x1,y1,x2,y2)`→ write "swipe … \n"              → read "OK …"
MCP tool `type_text(s)`      → write "text {s}\n"              → read "OK …"
MCP tool `wait_stable()`     → write "wait_stable\n"           → read "OK stable …"
```

`src/control/mod.rs::client_send(socket, command) -> io::Result<String>` is exactly this
one-request/one-response primitive, already used by the CLI client verbs; an in-process MCP server
can call it directly, or a separate process can speak the trivial line protocol over the socket.
Because the protocol is line-oriented text with `OK`/`ERR` replies and display-space coordinates,
the LLM never needs to know anything about HVF, virtio, or the scanout format.

**Recommended next step:** a small `aemulator-mcp` binary (or a `control mcp` subcommand) exposing
`screenshot / tap / swipe / type_text / key / wait_stable / launch` as MCP tools over stdio,
forwarding each to `client_send`. Optional hardening: a `region_hash x y w h` verb (assert a sub-rect
changed) and returning screenshot bytes inline (base64) so a tool call needs no shared filesystem.

---

## Hermeticity + regression

- **No new dependencies** — the PNG encoder is hand-rolled; the socket/REPL use only `std`.
- Default `cargo build --bin aemulator-hvf` is clean; `cargo test --lib` = **32/32** (Phase-1
  regression + new control/PNG unit tests: `parse_key`, `char_to_key`, `parse_ints`, CRC-32 &
  Adler-32 known-vectors, PNG header/chunk structure).
- The `--display` and headless boot paths are byte-identical (the new console-inject thread + the
  `DisplayBridge.console_tx` field are gated on `AEMU_CONTROL_NO_CONSOLE_STDIN`, set only in control
  mode).

## Networking + adb sideload (proven 2026-07-25)

> **Update 2026-07-27 — apps had NO internet until the NIC was renamed to `eth0`.** Everything
> below proves *host-level* NAT: raw sockets from the `shell` uid reach the internet, hostfwd binds,
> `adb` connects. None of that means **apps** have connectivity, and for months they did not.
> Android's `EthernetService` (EthernetTracker) only adopts an interface whose name fully matches its
> filter `(usb|eth)\d+`. Cuttlefish presents ours as **`buried_eth0`**, which does not match, so no
> `NetworkAgent` was ever created and `dumpsys connectivity` reported `Active default network: none`
> with an empty `Current Networks` — while `nc 1.1.1.1 443` from adb shell returned 0. Every
> host-side check passed; the product was offline.
>
> Fix (now automatic in `machine.rs::net_autoconfig_script`): rename the NIC to `eth0` (link down
> first) before configuring it. EthernetService then claims it, runs its own IpClient **DHCP against
> our slirp server**, and registers a network Android reports as `VALIDATED` — i.e. its own probe
> reached the internet. Verified 2026-07-27: `Active default network: 100`, DNS resolves
> (`google.com` → 74.125.136.139), ICMP replies in 7.8 ms. Guarded by two unit tests
> (`net_autoconfig_script_is_valid_shell`, `net_autoconfig_script_renames_nic_and_stays_quote_free`).
>
> Note this also corrects an old claim below: **ICMP/ping DOES work** through this libslirp build.

> **Update 2026-07-27 — `net` and `gfxstream` are no longer cargo features.** Both are compiled in
> unconditionally; `--features net` now errors. The features were removed because omitting one
> yields a binary that boots perfectly and is quietly crippled: a `--features gfxstream`-only
> release binary was being spawned by the Studio for a day, with a NIC that enumerated `eth0`, took
> its `10.0.2.15` lease and started adbd while dropping every packet. Build with a plain
> `cargo build --release --bin aemulator-hvf`. Mentions of `--features` below are historical.

Real user-mode NAT (libslirp, from the HostNet stage) plus a stable SWRENDER boot
was used to enable **adb over TCP** and sideload + launch a 335 MB APK (3DMark) entirely headless.
Everything below is OBSERVED, not asserted.

### The two prerequisites that were NOT obvious

1. **A GPU render path is REQUIRED for `pm`/`adb install` — headless (no `--gpu3d`, no `SWRENDER`)
   does NOT reach a usable system.** Booting `run-control64.sh` with neither flag leaves zygote
   aborting in `ZygoteInit.preload` (graphics driver) every ~5 s → a zygote↔SurfaceFlinger↔netd
   crash cascade (observed: 200 zygote starts, 190 netd starts, `boot_completed` 560 s overdue,
   interface TX frozen). This is the same wall PHASE4-STATUS §"Headless (no --gpu3d)" documents.
   **Fix: `SWRENDER=1 ./run-control64.sh`** (guest-CPU SwiftShader, default hermetic build, no host
   GPU) breaks the loop → **zygote stabilises (1 start), system_server forks, `sys.boot_completed=1`,
   BOOT_COMPLETED broadcasts, PackageManager lists 151 packages.** Only then can `pm` install.
   The control-plane screenshot captures the SwiftShader scanout headlessly (no window needed).

2. **Android policy routing — you MUST add `ip rule ... lookup main` or ALL TCP is `Network is
   unreachable`.** The renamed-unmanaged NIC comes up as `buried_eth0` (DOWN). Bringing it up
   (`ip link set up; ip addr add 10.0.2.15/24; ip route add default via 10.0.2.2 onlink`) is NOT
   enough: Android's `ip rule` catch-all at pref 32000 is `unreachable`, so the `main` table (with
   your default route) is never consulted — `ip route get 1.1.1.1` → *Network is unreachable*, and
   nothing transmits (only ARP). The gateway ARP DOES resolve on its own (`10.0.2.2` →
   `52:55:0a:00:02:02`, slirp's MAC). The missing step:

   ```sh
   ip rule add from all lookup main pref 31000
   ```

   With that rule the datapath works **both ways** — the earlier host→guest hostfwd failure (SYN
   arrived, no SYN-ACK) was the SAME bug: the guest couldn't route the SYN-ACK back to 10.0.2.2.
   netd reconciles the manual route/rule away between commands, so re-assert them right before use
   (a ~15 s keepalive loop held the route up across the 335 MB install).

### Full bring-up (as root over the control-plane `sh` verb; console runs as uid `shell`, `su 0`)

```sh
IF=buried_eth0
ip link set $IF up
ip addr replace 10.0.2.15/24 dev $IF
ip route replace default via 10.0.2.2 dev $IF onlink
ip rule add from all lookup main pref 31000            # <-- the load-bearing line
ip neigh replace 10.0.2.2 lladdr 52:55:0a:00:02:02 dev $IF nud permanent
setprop service.adb.tcp.port 5555 ; stop adbd ; start adbd
```

### adb-over-TCP quirk: port 5555 is in adb's emulator-console range → use a relay

`adb` treats an odd port in 5555–5585 as an emulator adbd (pairs it with a 5554 console), so
`adb connect localhost:5555` mis-registers as `emulator-5554`. A trivial host TCP relay
(`127.0.0.1:6555 → 127.0.0.1:5555`, `python3`, threaded) sidesteps it — `adb connect localhost:6555`.
(Both entries ultimately resolve to `device` here once routing is fixed; the relay just keeps it clean.)

### Observed results

```
# adb devices -l   (auth OK — device, not offline/unauthorized; ro.adb.secure=0 on userdebug)
emulator-5554  device product:aosp_cf_arm64_only_phone model:Cuttlefish_arm64_phone_64_bit_only
localhost:6555 device product:aosp_cf_arm64_only_phone model:Cuttlefish_arm64_phone_64_bit_only

# guest→host outbound proof (host `python3 -m http.server 8099`):
#   guest: GET /ping.txt HTTP/1.0 → "hello-from-host-…"
#   host httpd log: 127.0.0.1 - - "GET /ping.txt HTTP/1.0" 200

# adb install -r -g 3DMarkandroid-v2-6-5056.apk   (335 MB, over slirp NAT)
Performing Streamed Install
Success

# verify
$ adb shell pm list packages | grep -i futuremark
package:com.futuremark.dmandroid.application
$ adb shell pm path com.futuremark.dmandroid.application
package:/data/app/~~hSaclvHrAgKnD24uOBBfTg==/com.futuremark.dmandroid.application-…/base.apk
#   versionName=2.6.5056 versionCode=5056 minSdk=23 targetSdk=35 ; device SDK 37 / Android 17

# launch → foreground
$ adb shell am start -n com.futuremark.dmandroid.application/com.futuremark.flamenco.ui.splash.SplashPageActivity
$ adb shell dumpsys activity activities | grep ResumedActivity
  topResumedActivity= …/com.futuremark.flamenco.ui.splash.TutorialActivity   (pid running)
```

Screenshot (control-plane scanout capture, headless, SWRENDER): **`screenshots/3dmark-installed.png`**
— the 3DMark "Welcome" tutorial page (3DMARK logo + "the best way to test and compare the
performance of your smartphone and tablet" + SKIP), i.e. the app installed AND launched.

### AUTOMATIC on cold boot — no manual guest commands (verified 2026-07-25)

The full bring-up above is now applied automatically by a **VMM-side thread** — nothing is typed in
the guest. `src/machine.rs` spawns `net-autoconf` when `cfg.netdev && control_mode` (gate:
`AEMU_NET_AUTOCONF`, default on); it writes an idempotent bring-up script to `console_tx` on a timer
(20 s initial delay, then ~5 s ticks for the first ~2 min, then a 12 s idempotent keepalive) that
re-asserts the `lookup main` rule + default route indefinitely so netd can't reconcile connectivity
away. The console shell on this cf64 image runs as uid 2000 (`shell`), so the script escalates via
`su 0 sh -c "…"` (toybox `su` has NO `-c` flag — `su 0 -c` dies "failed to exec -c"; the wrapper is
`su 0 sh -c`).

**Cold-boot proof (all OBSERVED, `SWRENDER=1 ./run-control64.sh`, ZERO manual ip/route/rule/adbd
commands in the guest):**

```
# adb out of the box (host tool → guest adbd, via the 6555→5555 relay):
$ adb connect localhost:6555 ; adb devices -l
localhost:6555   device product:aosp_cf_arm64_only_phone model:Cuttlefish_arm64_phone_64_bit_only
$ adb -s localhost:6555 shell getprop sys.boot_completed        → 1

# guest network state — all applied by the VMM thread, nothing typed in-guest:
buried_eth0: <…,UP,LOWER_UP> … inet 10.0.2.15/24 scope global buried_eth0
ip rule:  31000: from all lookup main        (above the 32000 unreachable catch-all — load-bearing)
route:    default via 10.0.2.2 dev buried_eth0 onlink
props:    service.adb.tcp.port=5555 ; init.svc.adbd=running ; net.dns1=10.0.2.3

# guest→host through slirp NAT (host `python3 -m http.server 8099`; guest has nc, no wget):
$ adb shell "printf 'GET /ping.txt HTTP/1.0\r\n\r\n' | toybox nc 10.0.2.2 8099"
HTTP/1.0 200 OK … cold-boot-proof-1785035602        # exact host-written stamp
host httpd log: 127.0.0.1 - - "GET /ping.txt HTTP/1.0" 200

# SURVIVES netd — re-checked after 80 s, still up (re-assert loop beat reconciliation):
ip rule → 31000 lookup main still present ; default route still via 10.0.2.2
guest→host fetch → netd-survival-1785035646 (fresh stamp) ; httpd logged the GET
```

Screenshot (control-plane scanout, headless SWRENDER): **`screenshots/net-autoconfig.png`** — the
Android lock screen fully rendered via SwiftShader (clock 03:24, "Fri, Jul 17", battery 85%).
3DMark survived in `/data`: `pm list packages` → `package:com.futuremark.dmandroid.application`.

Regression (unchanged by the autoconfig): default build hermetic (`otool -L` = Hypervisor +
Apple/winit frameworks + libSystem/libiconv only — no libslirp/glib); `--features net` and
`net,gfxstream` build clean; `cargo test --lib` 32/32; Phase-1 Alpine boots (MIDR real 0x61 → 0x51
accepted, `virtio_blk … [vda] 131072 512-byte blocks`, no panic, initramfs rescue shell).

### Run invocation

```
SWRENDER=1 ./run-control64.sh                 # stable boot (system_server) + control + libslirp NAT + hostfwd tcp 5555
# host relay so adb avoids the emulator port range:
python3 /tmp/adb-relay.py                      # 127.0.0.1:6555 -> 127.0.0.1:5555
adb connect localhost:6555 ; adb -s localhost:6555 install -r -g <apk>
```

## Files

- `src/control/mod.rs`, `src/control/png.rs` — the control plane.
- `src/machine.rs` — `DisplayBridge.console_tx`; `spawn_console_inject_thread`; control-mode gate.
- `src/bin/aemulator-hvf.rs` — `control` subcommand + client verbs + `run_control_bc`.
- `src/display/mod.rs` — bridge destructure updated (window path unchanged).
- `run-control64.sh`, `prove-control64.sh` — harness + proof driver.
