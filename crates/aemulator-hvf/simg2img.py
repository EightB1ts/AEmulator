#!/usr/bin/env python3
# simg2img.py — hermetic Android sparse-image -> raw converter (macOS-native, no Linux host binary).
#
# The Cuttlefish img zip ships `super.img` as an Android *sparse* image (magic 0xed26ff3a). The
# cvd-host_package `simg2img` that normally unsparses it is a Linux aarch64 ELF and cannot run on
# macOS, so we reimplement the (simple, stable) sparse format here in pure Python. Output is written
# as a genuine sparse file on APFS (DONTCARE/zero runs are seek-skipped, never materialized), so the
# 7.5 GiB logical super.raw costs only the populated bytes on disk.
#
#   python3 simg2img.py <sparse.img> <raw.out>
#
# Format ref: system/core/libsparse/sparse_format.h. Chunk types: RAW 0xCAC1, FILL 0xCAC2,
# DONTCARE 0xCAC3, CRC32 0xCAC4.
import os, struct, sys

SPARSE_MAGIC = 0xED26FF3A


def convert(src, dst):
    with open(src, "rb") as f:
        hdr = f.read(28)
        (magic, ver_major, ver_minor, file_hdr_sz, chunk_hdr_sz, blk_sz,
         total_blks, total_chunks, image_checksum) = struct.unpack("<IHHHHIIII", hdr)
        if magic != SPARSE_MAGIC:
            print(f"ERROR: not a sparse image (magic {magic:#x}, want {SPARSE_MAGIC:#x})",
                  file=sys.stderr)
            return 2
        if file_hdr_sz > 28:
            f.read(file_hdr_sz - 28)  # skip any extra header bytes
        out_size = total_blks * blk_sz
        print(f"sparse: v{ver_major}.{ver_minor} blk_sz={blk_sz} total_blks={total_blks} "
              f"chunks={total_chunks} -> raw {out_size} bytes")

        with open(dst, "wb") as out:
            out.truncate(out_size)  # pre-size; holes stay sparse on APFS
            pos = 0  # byte offset into the raw output
            for ci in range(total_chunks):
                ch = f.read(chunk_hdr_sz)
                if len(ch) < 12:
                    print(f"ERROR: truncated chunk header at #{ci}", file=sys.stderr)
                    return 3
                chunk_type, _res, chunk_blks, total_sz = struct.unpack("<HHII", ch[:12])
                if chunk_hdr_sz > 12:
                    f.read(chunk_hdr_sz - 12)
                data_sz = total_sz - chunk_hdr_sz
                span = chunk_blks * blk_sz
                if chunk_type == 0xCAC1:  # RAW
                    out.seek(pos)
                    remaining = data_sz
                    while remaining > 0:
                        buf = f.read(min(8 * 1024 * 1024, remaining))
                        if not buf:
                            print("ERROR: EOF in RAW chunk", file=sys.stderr)
                            return 4
                        out.write(buf)
                        remaining -= len(buf)
                    pos += span
                elif chunk_type == 0xCAC2:  # FILL
                    fill = f.read(4)
                    val = struct.unpack("<I", fill)[0]
                    if val == 0:
                        pos += span  # zero fill -> leave as hole
                    else:
                        out.seek(pos)
                        block = fill * (blk_sz // 4)
                        remaining = span
                        while remaining > 0:
                            n = min(len(block), remaining)
                            out.write(block[:n])
                            remaining -= n
                        pos += span
                elif chunk_type == 0xCAC3:  # DONTCARE
                    pos += span  # skip -> hole
                elif chunk_type == 0xCAC4:  # CRC32
                    f.read(4)
                else:
                    print(f"ERROR: unknown chunk type {chunk_type:#x} at #{ci}", file=sys.stderr)
                    return 5
            out.truncate(out_size)
    print(f"wrote {dst} ({os.path.getsize(dst)} bytes logical)")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("usage: simg2img.py <sparse.img> <raw.out>", file=sys.stderr)
        sys.exit(1)
    sys.exit(convert(sys.argv[1], sys.argv[2]))
