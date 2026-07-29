#!/usr/bin/env bash
# third_party/regen-cereal.sh — regenerate gfxstream's host Vulkan "cereal" serialization code.
#
#   third_party/regen-cereal.sh --reproduce    # gate: generator + codegen patches + baseline == checkout
#   third_party/regen-cereal.sh --pristine     # bring-up only: generator alone, no codegen patches
#   third_party/regen-cereal.sh                # regenerate in place, into the gfxstream checkout
#   third_party/regen-cereal.sh --opcode-diff  # wire-compat gate: no EXISTING OP_vk* value may move
#
# WHY THIS EXISTS
# --------------
# gfxstream's guest->host Vulkan wire format is generated code, and two of its generated switches
# disagree: goldfish_vk_extension_struct_size() knows the byte size of 523 VkStructureTypes, while
# reservedunmarshal_extension_struct() can decode only 194 of them — and its default: arm is abort()
# of the whole VMM. The 329-type / 168-extension difference is the "cereal gap".
#
# The gap is NOT about struct complexity. cerealgenerator.py carries a hand-maintained
# SUPPORTED_FEATURES allow-list, and UNSUPPORTED_FEATURE_MODULES = {"goldfish_vk_extension_structs"}
# routes every UNLISTED feature to the size-table wrapper ONLY (see beginFeature()). Adding names to
# that list and regenerating is what closes the gap. This script is how we regenerate.
#
# WHICH GENERATOR, AND WHY NOT THE MESA ONE
# -----------------------------------------
# NOT third_party/mesa's codegen. The pinned mesa and pinned gfxstream are ~20 months apart and the
# OUTPUT CONTRACT differs: mesa emits vk_decoder / vk_decoder_snapshot module names and lowercase
# mesa-era headers ("vulkan_stream.h", "gfxstream/host/stream_utils.h") that exist NOWHERE in this
# gfxstream. It would not compile, for reasons unrelated to extensions.
#
# The right generator is a blob in gfxstream's OWN history, TP_GFXSTREAM_CODEGEN_REF — the commit
# titled "gfxstream: rerun codegen from external/mesa3d". Its very next commit deleted the directory
# as a duplicate, which is why it is not in the worktree. It emits VkDecoder / VkDecoderSnapshot /
# VkSubDecoder, matching the checkout.
#
# WHY --reproduce DOES NOT EXPECT A BYTE-IDENTICAL TREE
# ----------------------------------------------------
# Upstream HAND-EDITED the generated files after deleting the codegen. Five commits did it, all
# ancestors of the pin:
#   875145234 Keep VK_EXT_line_rasterization in codegen   (VkDecoder/VkDecoderSnapshot, by hand)
#   4d94b0813 Fix vkCmdSetLineStippleKHR decoding         (also edits codegen/.../vk.xml itself)
#   3e073ad03 Update auto-generated comments.             (banner only)
#   699158abd [Vulkan Snapshot]: avoid double boxing commandBuffer   (VkDecoderSnapshot, +264/-132)
#   b32fb06f0 [Vulkan Snapshot]: handle DescriptorSet allocate and update (VkDecoderSnapshot +77)
# So the pin's generated files are generated-PLUS-hand-work, and no generator revision can reproduce
# them. Chasing one through mesa's history is a dead end; this was checked.
#
# What --reproduce therefore asserts is the useful invariant: regenerating changes NOTHING except a
# recorded, reviewed baseline of that hand-work (plus our own patch hunks and the banner). The
# baseline lives next to the codegen patches as reproduce-baseline.diff. If the real diff stops
# matching the baseline, either the generator is wrong or someone hand-edited a generated file —
# both of which must stop the line.
#
# CRUCIALLY, the gap-defining files are reproduced essentially exactly: goldfish_vk_deepcopy.cpp and
# goldfish_vk_transform.cpp differ by BANNER ONLY, and goldfish_vk_extension_structs.cpp by two
# static_cast<size_t>(0) -> (size_t)0 casts. The hand-work is concentrated in VkDecoderSnapshot.cpp
# (495 lines) and VkDecoder.cpp (108). That is what makes regeneration safe for closing the gap.
#
# THE BANNER IS UNREPRODUCIBLE BY CONSTRUCTION
# --------------------------------------------
# cerealgenerator.py stamps each file with os.path.relpath() of its own argv. The checkout's banner
# encodes an AOSP superproject path ("../../../../../hardware/google/gfxstream/...") that cannot
# exist here, so banner lines are filtered out of every comparison below rather than chased.
set -uo pipefail

TP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$TP_DIR/MANIFEST.sh"
. "$TP_DIR/paths.sh"

C_B=$'\033[1m'; C_G=$'\033[32m'; C_Y=$'\033[33m'; C_R=$'\033[31m'; C_0=$'\033[0m'
step() { printf '\n%s==>%s %s%s%s\n' "$C_B" "$C_0" "$C_B" "$*" "$C_0"; }
ok()   { printf '%s  ✓%s %s\n' "$C_G" "$C_0" "$*"; }
warn() { printf '%s  ! %s%s\n' "$C_Y" "$*" "$C_0"; }
die()  { printf '%s  ✗ %s%s\n' "$C_R" "$*" "$C_0" >&2; exit 1; }

MODE="regen"
for a in "$@"; do
    case "$a" in
        --reproduce)        MODE="reproduce" ;;
        # Bring-up only: prove the recorded codegen IS the one that produced the checkout. Only
        # meaningful before any SUPPORTED_FEATURES additions exist in the checkout's cereal.
        --pristine)         MODE="pristine" ;;
        --opcode-diff)      MODE="opcode" ;;
        # Accept the CURRENT generator-vs-checkout delta as the reviewed baseline. Only legitimate
        # when the delta has been read and understood as upstream's post-codegen hand-work plus our
        # own patch hunks — it is the moral equivalent of bootstrap.sh --record-digest.
        --record-baseline)  MODE="record" ;;
        *) die "unknown argument '$a' (expected --reproduce, --pristine, --opcode-diff or --record-baseline)" ;;
    esac
done

CODEGEN_REF="${TP_GFXSTREAM_CODEGEN_REF:-}"
[ -n "$CODEGEN_REF" ] || die "TP_GFXSTREAM_CODEGEN_REF is not set in MANIFEST.sh"
CODEGEN_PATCH_DIR="$TP_DIR/patches/${TP_GFXSTREAM_CODEGEN_PATCHES:-gfxstream-codegen}"
BASELINE="$CODEGEN_PATCH_DIR/reproduce-baseline.diff"

[ -d "$TP_GFXSTREAM_SRC/.git" ] || die "no gfxstream checkout at $TP_GFXSTREAM_SRC (run bootstrap.sh)"

# The six generated pairs live under host/vulkan/cereal/common; the decoders under host/vulkan.
# addCppModule("common", ...) has no customAbsDir, so -o must be the cereal dir and the files land
# in <-o>/common/.
GEN_ROOT="$TP_GFXSTREAM_SRC"

# ---------------------------------------------------------------------------------------------
# clang-format. cereal/common/codegen.py does `shutil.which('clang-format')` and then
# `assert (clang_format_command is not None)` — with no clang-format the generator produces all 20
# files and THEN dies in endFile(), leaving a half-formatted tree. Resolve it up front.
# The VERSION does not matter: LLVM 22 and Apple 21 outputs were compared and differ only in the
# banner (which encodes argv). --style=file resolves gfxstream's own .clang-format by walking up
# from each file, so the output directory must sit under a directory that has one.
# ---------------------------------------------------------------------------------------------
resolve_clang_format() {
    local c
    for c in /opt/homebrew/opt/llvm/bin/clang-format /usr/local/opt/llvm/bin/clang-format; do
        [ -x "$c" ] && { printf '%s' "$c"; return 0; }
    done
    c="$(command -v clang-format 2>/dev/null)" && [ -n "$c" ] && { printf '%s' "$c"; return 0; }
    c="$(xcrun -f clang-format 2>/dev/null)" && [ -n "$c" ] && { printf '%s' "$c"; return 0; }
    return 1
}
CLANG_FORMAT="$(resolve_clang_format)" \
    || die "clang-format not found (tried brew llvm, PATH, xcrun). The codegen asserts on it."

PYTHON="$(command -v python3)"
[ -n "$PYTHON" ] || die "python3 not found"

# ---------------------------------------------------------------------------------------------
# Extract the era codegen + registry from gfxstream's object store into a scratch tree, then apply
# our codegen patch series to that scratch copy. The patches are NEVER applied to a checkout: they
# are an input to this developer tool, not a state of a dependency, so they stay outside
# bootstrap.sh's digest machinery by construction.
# ---------------------------------------------------------------------------------------------
# A FIXED scratch path, not mktemp. cerealgenerator.py stamps each file with os.path.relpath() of
# its own argv, so a random temp directory would leak a different absolute path into the banner on
# every run — which would make the recorded reproduce baseline unreproducible by construction.
# third_party/* is gitignored, so this stays out of the tree.
SCRATCH="$TP_DIR/.regen-cereal-scratch"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"

step "extracting codegen $CODEGEN_REF from the gfxstream object store"
mkdir -p "$SCRATCH/codegen"
git -C "$TP_GFXSTREAM_SRC" archive "$CODEGEN_REF" src/gfxstream/codegen \
    | tar -x -C "$SCRATCH/codegen" --strip-components=3 \
    || die "cannot extract $CODEGEN_REF:src/gfxstream/codegen — is the pin fetched?"
[ -f "$SCRATCH/codegen/scripts/genvk.py" ] || die "extracted codegen has no scripts/genvk.py"
[ -f "$SCRATCH/codegen/xml/vk_gfxstream.xml" ] || die "extracted codegen has no xml/vk_gfxstream.xml"
ok "codegen at $CODEGEN_REF ($(find "$SCRATCH/codegen/scripts" -name '*.py' | wc -l | tr -d ' ') python files)"

# The REGISTRY comes from the worktree, not the blob. Commit 4d94b0813 ("Fix vkCmdSetLineStippleKHR
# decoding") edited codegen/vulkan/vulkan-docs-next/xml/vk.xml by 69 lines AFTER the codegen blob,
# and the in-tree copy is what the checkout's banner names. Using the blob's registry instead moves
# an opcode (vkCmdSetLineStippleEXT -> KHR) and inflates the reproduce delta by ~90 lines.
#
# That in-tree copy is NO LONGER gfxstream's. It is Vulkan-Docs $TP_VULKAN_DOCS_TAG run through
# third_party/downgrade-registry.py, which undoes six upstream registry refactors the era scripts
# cannot parse. Read that file before touching the registry; a bump also requires bumping
# common/vulkan/include/**/*.h to the matching Vulkan-Headers tag, or nothing compiles.
REGISTRY="$TP_GFXSTREAM_SRC/codegen/vulkan/vulkan-docs-next/xml/vk.xml"
[ -f "$REGISTRY" ] || die "no in-tree registry at $REGISTRY"
ok "registry: in-tree vk.xml ($(wc -c < "$REGISTRY" | tr -d ' ') bytes)"

# --pristine runs the codegen with NO codegen patches. That answers the bring-up question — "are we
# driving the right generator at all?" — and is only meaningful against a checkout whose cereal has
# not had features added yet. It is kept because it is how the generator was originally identified.
#
# --reproduce applies the patches, because once features ARE added the checkout legitimately
# contains them; the useful standing invariant is the stronger one:
#     generator + codegen patches + recorded baseline == the checkout
# which is what catches a hand-edit to a generated file, or a pin bump that moves the codegen.
if [ "$MODE" = "pristine" ]; then
    warn "pristine mode: codegen patches NOT applied (tests the generator alone)"
elif [ -d "$CODEGEN_PATCH_DIR" ]; then
    shopt -s nullglob
    for p in "$CODEGEN_PATCH_DIR"/*.patch; do
        # `patch`, never `git apply`. $SCRATCH lives under third_party/, which is INSIDE this repo's
        # worktree, so `git apply` resolves the patch's paths against the AEmulator repo root
        # instead of the scratch dir — finds nothing to do, changes nothing, and EXITS 0. That
        # silent no-op shipped a "✓ applied" log next to a completely unpatched generator.
        before="$(find "$SCRATCH/codegen" -type f -exec shasum {} + | shasum | cut -d' ' -f1)"
        patch -d "$SCRATCH/codegen" -p1 -s --no-backup-if-mismatch < "$p" \
            || die "codegen patch does not apply: $(basename "$p")"
        after="$(find "$SCRATCH/codegen" -type f -exec shasum {} + | shasum | cut -d' ' -f1)"
        [ "$before" != "$after" ] \
            || die "codegen patch changed NOTHING: $(basename "$p") — it applied as a no-op, which
   means its paths do not match the extracted tree (expected paths like a/scripts/cerealgenerator.py)"
        ok "applied $(basename "$p")"
    done
    shopt -u nullglob
fi

# ---------------------------------------------------------------------------------------------
# Run the generator.
#   CEREAL_VARIANT=host          -> never register the guest modules (this tree has no guest/vulkan_enc)
#   GFXSTREAM_HOST_DECODER_DIR   -> addHostModule() SILENTLY SKIPS EVERY MODULE if this does not
#                                   exist. A typo yields a successful-looking run that writes
#                                   nothing, so the skip message is treated as fatal below.
#   GFXSTREAM_SCRIPTS_DIR        -> non-optional: PyScript(customAbsDir=Path(self.host_script_destination))
#                                   raises TypeError on Path(None). It regenerates the TRACKED file
#                                   scripts/print_gfx_logs/vulkan_printer.py — expected in the delta.
# ---------------------------------------------------------------------------------------------
run_codegen() {
    local out="$1" log="$2" err="$3"
    mkdir -p "$out/host/vulkan/cereal/common" "$out/scripts/print_gfx_logs"
    # --style=file walks UP from each generated file, so the output tree needs a .clang-format.
    cp "$TP_GFXSTREAM_SRC/.clang-format" "$out/.clang-format"
    ( cd "$SCRATCH/codegen/scripts" \
      && PATH="$(dirname "$CLANG_FORMAT"):$PATH" \
         CEREAL_VARIANT=host \
         GFXSTREAM_HOST_DECODER_DIR="$out/host/vulkan" \
         GFXSTREAM_SCRIPTS_DIR="$out/scripts" \
         "$PYTHON" genvk.py \
            -registry "$REGISTRY" \
            -registryGfxstream "$SCRATCH/codegen/xml/vk_gfxstream.xml" \
            -o "$out/host/vulkan/cereal" cereal \
            > "$log" 2> "$err" ) || { sed -n '1,40p' "$err" >&2; die "genvk.py failed"; }

    # A silent skip is worse than a crash: it looks like success and writes nothing.
    if grep -qiE 'not found.*skipping' "$log"; then
        sed -n '1,20p' "$log" >&2
        die "codegen skipped modules — GFXSTREAM_HOST_DECODER_DIR is wrong"
    fi
    # Opcodes are sha256(name)-derived with linear probing on collision; a probe silently renumbers
    # a function and breaks the wire against the PREBUILT guest ICD. The generator prints the exact
    # KNOWN_FUNCTION_OPCODES line to paste.
    if grep -q 'Hash collision occurred on function' "$err"; then
        grep 'Hash collision occurred on function' "$err" >&2
        die "opcode hash collision — add the printed entry to the codegen patch's cereal/marshalingdefs.py KNOWN_FUNCTION_OPCODES"
    fi
    [ -f "$out/host/vulkan/cereal/common/goldfish_vk_reserved_marshaling.cpp" ] \
        || die "codegen produced no reserved_marshaling — nothing was generated"
}

# BANNER GRAFTING.
# cerealgenerator.py stamps every generated file with a comment block embedding os.path.relpath() of
# its own argv, wrapped by clang-format at 100 columns — so both its CONTENT and its LINE COUNT vary
# with where the generator ran. The checkout's copy encodes an AOSP superproject path
# ("../../../../../hardware/google/gfxstream/...") that cannot exist here.
#
# Merely FILTERING banner lines out of a diff is not enough, because the differing line counts shift
# every subsequent line number and the resulting diff cannot be applied. Instead we GRAFT: copy the
# checkout's banner verbatim into the freshly generated file. After grafting, the two files differ
# only by real content, line numbers line up, and a plain unified diff both reads correctly AND
# applies cleanly — which is what lets the recorded baseline be re-applied automatically after every
# regeneration (see the `regen` mode; upstream's hand-written vkCmdSetLineStippleEXT support lives
# there, and losing it silently deletes OP_vkCmdSetLineStippleEXT — a wire break against the
# prebuilt guest ICD).
#
# Block shape:
#     // Autogenerated module <name>
#     //
#     // (impl|header) generated by <...argv, wrapped over 1..4 lines...>
#     //
#     // Please do not modify directly;
#     // re-run <...script path...>,
graft_banner() {   # $1 = donor (checkout) file, $2 = file to rewrite in place
    "$PYTHON" - "$1" "$2" <<'PY'
import re, sys
BLOCK = re.compile(r'^// \((?:impl|header)\) generated by.*?^// re-run .*?$', re.S | re.M)
donor = open(sys.argv[1], encoding='utf-8', errors='replace').read()
target_path = sys.argv[2]
target = open(target_path, encoding='utf-8', errors='replace').read()
m = BLOCK.search(donor)
if m:
    # Only graft when the target has a banner of its own to replace; some header-only modules
    # (suppressFeatureGuards) legitimately lack one.
    if BLOCK.search(target):
        target = BLOCK.sub(lambda _: m.group(0), target, count=1)
        open(target_path, 'w', encoding='utf-8').write(target)
PY
}

generated_files() { ( cd "$1" && find . -type f ! -name '.clang-format' | sed 's|^\./||' | sort ); }

graft_all() {   # $1 = generated tree — make every file's banner match the checkout's
    local out="$1" f
    for f in $(generated_files "$out"); do
        [ -f "$GEN_ROOT/$f" ] && graft_banner "$GEN_ROOT/$f" "$out/$f"
    done
}

# A real, applicable unified diff: freshly generated -> checkout. Applying it to a fresh generation
# reconstructs the checkout's post-codegen hand-work.
real_diff() {   # $1 = generated tree (already grafted)
    local out="$1" f
    for f in $(generated_files "$out"); do
        [ -f "$GEN_ROOT/$f" ] || { printf '### NEW FILE (not in checkout): %s\n' "$f"; continue; }
        diff -u --label "a/$f" --label "b/$f" "$out/$f" "$GEN_ROOT/$f"
    done
}

# Re-apply upstream's post-codegen hand-work (and our own patch hunks) on top of a fresh generation.
# Without this, regeneration silently deletes things nobody re-derives — most sharply
# OP_vkCmdSetLineStippleEXT, whose loss is a wire break against the PREBUILT guest ICD.
#
# `patch` rather than `git apply`: once new extensions are added the surrounding line numbers shift,
# and patch's fuzz absorbs that where git apply would reject outright. A rejected hunk is fatal —
# it means the hand-work no longer has a home and must be re-derived by a human.
apply_baseline() {   # $1 = generated tree (already grafted)
    local out="$1"
    [ -f "$BASELINE" ] || die "no recorded baseline at $BASELINE (run --record-baseline)"
    if ! ( cd "$out" && patch -p1 -s --no-backup-if-mismatch -F 3 < "$BASELINE" ); then
        die "the recorded baseline no longer applies to freshly generated code.
   This means upstream's post-codegen hand-work (VkDecoderSnapshot / VkDecoder line-stipple +
   snapshot commits) has lost its anchor, OR a feature you added rewrote the same region.
   Re-derive it by hand; do NOT drop it — losing it deletes OP_vkCmdSetLineStippleEXT."
    fi
    ok "re-applied the recorded post-codegen hand-work"
}

case "$MODE" in
record)
    step "recording the reproduce baseline"
    OUT="$SCRATCH/out"
    run_codegen "$OUT" "$SCRATCH/gen.out" "$SCRATCH/gen.err"
    graft_all "$OUT"
    mkdir -p "$CODEGEN_PATCH_DIR"
    real_diff "$OUT" > "$BASELINE"
    ok "wrote $BASELINE ($(wc -l < "$BASELINE" | tr -d ' ') lines)"
    printf '  changed files:\n'
    grep -E '^\+\+\+ b/' "$BASELINE" | sed 's|^+++ b/|    |'
    warn "READ THIS DIFF before committing it — it is the definition of 'expected drift',"
    warn "and 'regen' re-applies it verbatim on top of every future regeneration."
    ;;

reproduce|pristine)
    step "$MODE: generating with the recorded codegen, comparing against the checkout"
    OUT="$SCRATCH/out"
    run_codegen "$OUT" "$SCRATCH/gen.out" "$SCRATCH/gen.err"
    graft_all "$OUT"
    real_diff "$OUT" > "$SCRATCH/real.diff"
    n=$(grep -cE '^[-+][^-+]' "$SCRATCH/real.diff" || true)
    ok "generated $(generated_files "$OUT" | wc -l | tr -d ' ') files; $n differing lines vs the checkout (banner grafted)"

    if [ ! -f "$BASELINE" ]; then
        warn "no recorded baseline at $BASELINE"
        warn "review the diff, then accept it with:  bash $0 --record-baseline"
        die "reproduce baseline not recorded yet"
    fi

    if diff -q "$BASELINE" "$SCRATCH/real.diff" >/dev/null 2>&1; then
        ok "reproduce CLEAN — output matches the checkout modulo the recorded baseline"
        exit 0
    fi
    warn "reproduce DRIFTED from the recorded baseline:"
    diff -u "$BASELINE" "$SCRATCH/real.diff" | head -80
    die "either the generator changed, or someone hand-edited a generated file in the checkout"
    ;;

opcode)
    step "opcode stability: no EXISTING OP_vk* value may move"
    OUT="$SCRATCH/out"
    run_codegen "$OUT" "$SCRATCH/gen.out" "$SCRATCH/gen.err"
    graft_all "$OUT"
    apply_baseline "$OUT"
    H="$GEN_ROOT/host/vulkan/cereal/common/goldfish_vk_marshaling.h"
    N="$OUT/host/vulkan/cereal/common/goldfish_vk_marshaling.h"
    grep -E '^#define OP_vk' "$H" | sort > "$SCRATCH/op.old"
    grep -E '^#define OP_vk' "$N" | sort > "$SCRATCH/op.new"
    # A name whose VALUE changed appears in old-with-one-value and new-with-another. Compare by name.
    awk '{print $2}' "$SCRATCH/op.old" | sort > "$SCRATCH/nm.old"
    awk '{print $2}' "$SCRATCH/op.new" | sort > "$SCRATCH/nm.new"
    moved=0
    while read -r name; do
        o=$(grep -E "^#define $name " "$SCRATCH/op.old" | awk '{print $3}')
        n=$(grep -E "^#define $name " "$SCRATCH/op.new" | awk '{print $3}')
        if [ -n "$o" ] && [ -n "$n" ] && [ "$o" != "$n" ]; then
            printf '  %s  %s -> %s\n' "$name" "$o" "$n"; moved=1
        fi
    done < "$SCRATCH/nm.old"
    added=$(comm -13 "$SCRATCH/nm.old" "$SCRATCH/nm.new" | wc -l | tr -d ' ')
    removed=$(comm -23 "$SCRATCH/nm.old" "$SCRATCH/nm.new" | wc -l | tr -d ' ')
    printf '  opcodes: %s existing, +%s added, -%s removed\n' \
        "$(wc -l < "$SCRATCH/nm.old" | tr -d ' ')" "$added" "$removed"
    [ "$removed" -gt 0 ] && { comm -23 "$SCRATCH/nm.old" "$SCRATCH/nm.new" | sed 's/^/    -/'; }
    [ "$moved" = 1 ] && die "OPCODE REGRESSION — this is a wire break against the prebuilt guest ICD"
    [ "$removed" -gt 0 ] && die "opcodes disappeared — wire break against the prebuilt guest ICD"
    ok "no existing opcode moved (+$added new)"
    ;;

regen)
    step "regenerating in place into $GEN_ROOT"
    OUT="$SCRATCH/out"
    run_codegen "$OUT" "$SCRATCH/gen.out" "$SCRATCH/gen.err"
    graft_all "$OUT"
    apply_baseline "$OUT"
    for f in $(generated_files "$OUT"); do
        [ -f "$GEN_ROOT/$f" ] || { warn "new file not in checkout, skipping: $f"; continue; }
        cp "$OUT/$f" "$GEN_ROOT/$f"
    done
    ok "wrote $(generated_files "$OUT" | wc -l | tr -d ' ') generated files into the checkout"
    warn "next: bash $0 --opcode-diff   (wire-compat gate)"
    warn "      cargo run --bin aemu-cereal-gap -- generate ... > host/vulkan/VkDecoderCerealGap.inc"
    ;;
esac
