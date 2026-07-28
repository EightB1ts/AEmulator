#!/usr/bin/env bash
# prep-debrand.sh — Phase 5 (stealth): in-place, byte-length-preserving de-brand of the build.prop
# identity in the EROFS logical partitions we CAN safely edit without a repack.
#
# Only the ODM + SYSTEM_EXT build.props are FLAT_INLINE (uncompressed) in this image, so their OEM
# identity strings (brand/device/manufacturer/model/name + the per-partition fingerprint) are
# swapped for the aemulator-identity DeviceProfile (Xiaomi 13 fuxi / Xiaomi / 2211133C) with the
# file length held constant — no EROFS repack, no verity fixup. The load-bearing SYSTEM / PRODUCT /
# VENDOR build.props are EROFS-COMPRESSED (datalayout 3) and are handled by the (heavier, boot-risky)
# repack pass, not here — see PHASE5-STATUS.md § structural-remaining.
#
# Idempotent + safe to run before every boot: an already-de-branded build.prop is left untouched.
#
#   ./prep-debrand.sh [super.raw.img]
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SUPER="${1:-$HERE/testdata/cf64/img/super.raw.img}"

for part_path in odm_a:/etc/build.prop system_ext_a:/etc/build.prop; do
    part="${part_path%%:*}"
    path="${part_path#*:}"
    python3 "$HERE/debrand_buildprop_inplace.py" "$SUPER" "$part" "$path"
done
