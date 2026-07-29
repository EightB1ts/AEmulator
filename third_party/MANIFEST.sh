#!/usr/bin/env bash
# third_party/MANIFEST.sh — the single source of truth for every out-of-tree build dependency.
#
# WHY THIS FILE EXISTS. These three trees used to live in a hand-managed `SampleRepos/` directory
# alongside five unrelated reference checkouts, at whatever revision each happened to be fetched at,
# with our gfxstream changes existing ONLY as uncommitted edits inside a foreign git repo. That
# arrangement lost work twice in one session: moving the repo silently broke the gfxstream build tree
# (a stale CMake cache pointing at the old absolute path), and the gfxstream patches were one
# `git checkout` away from being unrecoverable. Everything about a dependency that we must be able to
# reproduce now lives here, in-tree and reviewable: where it comes from, exactly which commit, and
# which patches we carry on top.
#
# SOURCE THIS, do not duplicate it. Shell:  . third_party/MANIFEST.sh
# Path resolution for consumers (build.rs, CI, run scripts):  third_party/paths.sh
#
# UPDATING A PIN: change the SHA here, run `third_party/bootstrap.sh`, and re-run
# `ci/hvf-doctor.sh`. If a patch no longer applies, bootstrap fails loudly rather than building
# something that silently lost our changes.

# ---------------------------------------------------------------------------------------------
# Host floor. KosmicKrisp is built on Metal APIs that only exist on macOS 26+, and with MoltenVK
# removed there is nothing to fall back to — so this is a hard requirement, not a preference. It
# lived only in ci/hvf-doctor.sh, which meant a bootstrap on an older host cheerfully spent 45
# minutes BUILDING a driver the doctor then rejected. Recorded here so both agree by construction.
# ---------------------------------------------------------------------------------------------
TP_MACOS_MIN_MAJOR="26"

# ---------------------------------------------------------------------------------------------
# gfxstream — the guest<->host graphics protocol decoder. We carry REAL patches on this one; see
# third_party/patches/gfxstream/. Without them the host renderer cannot drive a conformant
# (non-portability) Vulkan driver and has no in-Vulkan present path at all.
# ---------------------------------------------------------------------------------------------
TP_GFXSTREAM_NAME="gfxstream"
TP_GFXSTREAM_URL="https://github.com/BlissOS/platform_hardware_google_gfxstream"
TP_GFXSTREAM_SHA="5c5fe539389dc4fc6b0566f8f9785285f0144af9"
TP_GFXSTREAM_PATCHES="gfxstream"   # subdir of third_party/patches, applied in sorted order

# The codegen that produced host/vulkan/cereal/** and host/vulkan/VkDecoder*, used by
# third_party/regen-cereal.sh. It is a REF INTO THE GFXSTREAM CHECKOUT'S OWN HISTORY, not a
# separate dependency — the commit titled "gfxstream: rerun codegen from external/mesa3d", whose
# very next commit ("nuke duplicate codegen directory [again]") deleted the directory, which is why
# it is absent from the worktree. regen-cereal.sh reads it with `git archive`.
#
# It is deliberately NOT third_party/mesa's copy of the same generator: the pinned mesa is ~20 months
# newer and its output contract changed (module names vk_decoder / vk_decoder_snapshot, and
# lowercase mesa-era includes like "vulkan_stream.h" that exist nowhere in this gfxstream). Running
# it produces files that do not compile, for reasons unrelated to extensions.
TP_GFXSTREAM_CODEGEN_REF="838dd1d61"
TP_GFXSTREAM_CODEGEN_PATCHES="gfxstream-codegen"

# The Vulkan REGISTRY the codegen above is driven with, and the C headers the result is compiled
# against. Both live inside the gfxstream checkout and are therefore carried by the patch series;
# these two variables exist so the provenance is stated somewhere a human reads, not only inferable
# from a 13 MB diff.
#
#   codegen/vulkan/vulkan-docs-next/xml/vk.xml      <- Vulkan-Docs at this tag, run through
#                                                      third_party/downgrade-registry.py
#   common/vulkan/include/{vulkan,vk_video}/*.h     <- Vulkan-Headers at this tag, verbatim
#                                                      (*.hpp deliberately left at the vendored
#                                                       version — see downgrade-registry.py)
#
# THEY MUST MOVE TOGETHER. The generated cereal names types by the registry's spelling, so a bumped
# registry against stale headers fails to compile on every promoted name. gfxstream shipped v1.3.269
# registry + v1.3.267 headers; AEmulator moved both to v1.4.348 so the 26 device extensions the host
# driver advertises but the old registry had never heard of became decodable instead of hidden.
TP_VULKAN_DOCS_TAG="v1.4.348"
TP_VULKAN_HEADERS_TAG="v1.4.348"

# Digest of the checkout's ENTIRE working-tree delta against the pinned SHA above: `git diff HEAD`
# (spelled with every formatting knob pinned) plus one `?? <blob-sha> <path>` line per untracked file.
#
# WHY IT EXISTS. The patch series is verified with `git apply --check --reverse`, which proves each
# hunk is PRESENT and nothing more. An edit made OUTSIDE every hunk's context reverse-applies exactly
# as cleanly — so a hand-edit to host/vulkan/VkDecoderGlobalState.cpp used to ship into the built
# dylib with `bootstrap.sh --verify` (and therefore ci/hvf-doctor.sh, and therefore ci/check.sh)
# still green. That matters because the patch file is the ONLY record of our host C++ changes.
# This value moves for any byte of difference anywhere in the tree, hunk or not.
#
# RE-RECORD IT whenever the patch series legitimately changes — the digest is a function of the
# series, so it goes stale on every regeneration:
#     bash third_party/bootstrap.sh --record-digest      # recomputes and rewrites the line below
TP_GFXSTREAM_TREE_DIGEST="622ef45ac68fdd6fb84b3dd2f83b1dceb1a657e0"

# ---------------------------------------------------------------------------------------------
# aemu — aemu-base / aemu-host-common. A build-time dependency of gfxstream's SYSTEM dependency
# resolution (it emits the .pc files gfxstream's cmake looks for). Carried unpatched.
# ---------------------------------------------------------------------------------------------
TP_AEMU_NAME="aemu"
TP_AEMU_URL="https://android.googlesource.com/platform/hardware/google/aemu"
TP_AEMU_SHA="3c1ced8a369417db591eb7cd083af5bb2c317975"
TP_AEMU_PATCHES=""

# ---------------------------------------------------------------------------------------------
# Mesa — built for the `kosmickrisp` Vulkan driver ONLY (LunarG's Vulkan-on-Metal). This is the
# host Vulkan driver; MoltenVK is gone and there is no fallback, so this is not optional.
#
# Pinned to a `main` revision on purpose, NOT a release branch: KK_WORKAROUND_15 (2026-07-14) fixes
# an MSL miscompile that only manifests on macOS 27, which is what we run. Carried unpatched.
# ---------------------------------------------------------------------------------------------
TP_MESA_NAME="mesa"
TP_MESA_URL="https://gitlab.freedesktop.org/mesa/mesa.git"
TP_MESA_SHA="4f4b73dfe58171865f95e8ef1b52236ad341dea0"
# Widens the GUEST Vulkan ICD's hardcoded device-extension allow-list. Boot-validated: guest-visible
# device extensions 89 -> 115. See the patch's own comment for the two names that MUST stay out and
# for the enumerate-vs-validate asymmetry that makes adding names dangerous.
TP_MESA_PATCHES="mesa"

# Iteration order for bootstrap. aemu before gfxstream: gfxstream's configure consumes aemu's .pc.
TP_DEPS="AEMU GFXSTREAM MESA"
