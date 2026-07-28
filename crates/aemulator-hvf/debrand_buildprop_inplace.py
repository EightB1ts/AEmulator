#!/usr/bin/env python3
# debrand_buildprop_inplace.py — Phase 5 (stealth): byte-length-preserving in-place rewrite of a
# build.prop inside an EROFS logical partition of a liblp `super.raw.img`.
#
# Swaps the Cuttlefish OEM identity strings (brand/device/manufacturer/model/name + the per-partition
# fingerprint) for the aemulator-identity DeviceProfile values (Xiaomi 13 `fuxi` / Xiaomi /
# 2211133C), WITHOUT changing the file's byte length — so the EROFS inode (i_size, datalayout,
# block map) is untouched and no repack / verity fixup is needed. The new content is always SHORTER
# than the Cuttlefish original (the generic/vsoc/aosp_cf strings are long), so the tail is padded
# with a comment line to the exact original length. Idempotent: a file already carrying the Xiaomi
# brand is left alone.
#
# ONLY works on FLAT_PLAIN / FLAT_INLINE (uncompressed) inodes — the compressed system/product/vendor
# build.props are handled by the repack path, not this one.
#
#   python3 debrand_buildprop_inplace.py <super.raw.img> <partition> <path-in-partition>
#     e.g. python3 debrand_buildprop_inplace.py img/super.raw.img odm_a /etc/build.prop
import os, re, struct, sys

# ---- crc32c (Castagnoli) for EROFS SB_CHKSUM, only used if the edit touches block 0 ----
POLY = 0x82F63B78
_tbl = []
for n in range(256):
    c = n
    for _ in range(8):
        c = (c >> 1) ^ POLY if (c & 1) else (c >> 1)
    _tbl.append(c)


def crc32c(buf, crc=0xFFFFFFFF):
    for b in buf:
        crc = (crc >> 8) ^ _tbl[(crc ^ b) & 0xFF]
    return crc & 0xFFFFFFFF


# ---- DeviceProfile identity (aemulator-identity reference_profile: Xiaomi 13 fuxi / SM8550) ----
BRAND = "Xiaomi"
DEVICE = "fuxi"
MANUFACTURER = "Xiaomi"
MODEL = "2211133C"
NAME = "fuxi"


def debrand_text(text):
    """Rewrite the identity lines. Keeps the running build's version/id/incremental/tags (so the
    fingerprint stays internally coherent with SDK 37) and only swaps the OEM identity + the
    generic/vsoc/aosp_cf/Cuttlefish/Google strings. Returns (new_text, changed)."""
    lines = text.split("\n")
    out = []
    changed = False
    for ln in lines:
        orig = ln
        m = re.match(r"^(ro\.product\.[a-z_]+\.)(brand|device|manufacturer|model|name)=(.*)$", ln)
        if m:
            attr = m.group(2)
            val = {
                "brand": BRAND,
                "device": DEVICE,
                "manufacturer": MANUFACTURER,
                "model": MODEL,
                "name": NAME,
            }[attr]
            ln = f"{m.group(1)}{attr}={val}"
        elif re.match(r"^ro\.[a-z_]+\.build\.fingerprint=", ln):
            # brand/name/device:release/id/incremental:type/tags — keep the trailing 5 fields from
            # the original (release/id/incremental:type/tags) so nothing but the OEM head changes.
            key, _, cur = ln.partition("=")
            tail = cur.split(":", 1)[1] if ":" in cur else "17/UNKNOWN/0:user/release-keys"
            # cur = brand/name/device:release/id/incremental:type/tags
            # split off the "brand/name/device" head, keep ":release/..." tail intact
            head_end = cur.find(":")
            rest = cur[head_end:] if head_end >= 0 else ":" + tail
            ln = f"{key}={BRAND}/{NAME}/{DEVICE}{rest}"
        # neutralise the SoC identity when present (vendor build.prop) — harmless on others.
        elif ln.startswith("ro.soc.manufacturer="):
            ln = "ro.soc.manufacturer=QTI"
        elif ln.startswith("ro.soc.model="):
            ln = "ro.soc.model=SM8550"
        elif ln.startswith("ro.product.board="):
            ln = "ro.product.board=fuxi"
        elif ln.startswith("ro.board.platform="):
            ln = "ro.board.platform=kalama"
        if ln != orig:
            changed = True
        out.append(ln)
    return "\n".join(out), changed


def main():
    super_path, part, path = sys.argv[1], sys.argv[2], sys.argv[3]
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import erofs_extract as E

    fd = os.open(super_path, os.O_RDWR)
    base, _ = E.lp_partition_extent(fd, super_path, part)
    ero = E.Erofs(fd, base)
    ino = ero.resolve(path)
    dl = ino["datalayout"]
    if dl not in (0, 2):
        print(f"[skip] {part}:{path} datalayout={dl} (compressed) — repack path required")
        return
    size = ino["size"]
    data = ero.read_data(ino, size)
    text = data.decode("utf-8", "replace")

    if BRAND in text and "generic" not in text and "vsoc" not in text:
        print(f"[noop] {part}:{path} already de-branded")
        return

    new_text, changed = debrand_text(text)
    if not changed:
        print(f"[noop] {part}:{path} no identity lines matched")
        return
    new_bytes = new_text.encode("utf-8")
    if len(new_bytes) > size:
        raise SystemExit(f"new content {len(new_bytes)} > original {size}; cannot preserve length")
    pad = size - len(new_bytes)
    if pad:
        # A fresh comment line of '#' padding — parser ignores it; exact byte count.
        if not new_bytes.endswith(b"\n"):
            new_bytes += b"\n"
            pad -= 1
        new_bytes += b"#" * pad
    assert len(new_bytes) == size, (len(new_bytes), size)

    # Physical write. FLAT_INLINE: full blocks at raw_blk, tail inline. FLAT_PLAIN: all at raw_blk.
    blksz = ero.blksz
    writes = []  # (absolute_offset, bytes)
    if dl == 0:  # FLAT_PLAIN
        writes.append((base + ino["raw_blk"] * blksz, new_bytes))
    else:  # FLAT_INLINE
        full = size // blksz * blksz
        if full:
            writes.append((base + ino["raw_blk"] * blksz, new_bytes[:full]))
        writes.append((base + ino["inline_off"], new_bytes[full:]))

    touches_block0 = any(off < base + blksz for off, b in writes)
    for off, b in writes:
        os.pwrite(fd, b, off)

    # Recompute EROFS superblock crc32c if we wrote into block 0 (superblock block). SB_CHKSUM
    # (feature_compat bit 0) is crc32c over [1024 : blksz] with the 4-byte csum field (@1024+4)
    # zeroed, NO final inversion — matches erofs_superblock_csum_verify / prep-super.sh.
    block0 = bytearray(os.pread(fd, blksz, base))
    feat_compat = struct.unpack("<I", block0[1024 + 8 : 1024 + 12])[0]
    if touches_block0 and (feat_compat & 0x1):
        region = bytearray(block0[1024:blksz])
        region[4:8] = b"\0\0\0\0"
        csum = crc32c(bytes(region))
        os.pwrite(fd, struct.pack("<I", csum), base + 1024 + 4)
        print(f"[crc] recomputed EROFS SB_CHKSUM = {csum:#010x}")

    os.fsync(fd)
    print(f"[ok] {part}:{path} de-branded in place ({len(new_bytes)} B, pad={pad})")


if __name__ == "__main__":
    main()
