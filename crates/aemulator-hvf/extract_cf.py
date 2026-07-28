#!/usr/bin/env python3
# extract_cf.py — derive the Cuttlefish boot artifacts the aemulator-hvf `run-android` binary needs
# (kernel, initramfs.cf.gz, bootconfig.txt) from the Android boot images in <cf>/img, WITHOUT the
# Linux-only unpack_bootimg from the host package. Pure-Python parse of the (stable) Android
# boot_img_hdr v4 + vendor_boot_img_hdr v4 formats.
#
#   python3 extract_cf.py <cf-dir>
#
# Emits into <cf-dir>:
#   kernel                     raw arm64 Image  (boot.img kernel section)
#   init_boot_ramdisk.cpio.gz  init_boot.img ramdisk (LZ4-legacy, already compressed)
#   vendor_ramdisk.cpio.gz     vendor_boot.img vendor-ramdisk section
#   initramfs.cf.gz            init_boot_ramdisk ++ vendor_ramdisk  (what the binary loads)
#   bootconfig.txt             vendor_boot.img trailing bootconfig section
import os, struct, sys


def align(x, a):
    return (x + a - 1) // a * a


def parse_boot(path):
    """Android boot_img_hdr v3/v4 (fixed 4096 page). Returns (kernel_bytes, ramdisk_bytes)."""
    d = open(path, 'rb').read()
    assert d[:8] == b'ANDROID!', f"{path}: bad boot magic {d[:8]!r}"
    kernel_size, ramdisk_size = struct.unpack('<II', d[8:16])
    PS = 4096
    off = PS
    kernel = d[off:off + kernel_size]
    off += align(kernel_size, PS)
    ramdisk = d[off:off + ramdisk_size]
    return kernel, ramdisk


def parse_vendor_boot(path):
    """vendor_boot_img_hdr v4. Returns (vendor_ramdisk_bytes, bootconfig_bytes)."""
    d = open(path, 'rb').read()
    assert d[:8] == b'VNDRBOOT', f"{path}: bad vendor magic {d[:8]!r}"
    (hdr_ver, page_size, kaddr, raddr, vendor_ramdisk_size) = struct.unpack('<IIIII', d[8:28])
    # cmdline[2048]@28, tags_addr@2076, name[16]@2080, header_size@2096, dtb_size@2100,
    # dtb_addr(u64)@2104, vendor_ramdisk_table_size@2112, entry_num@2116, entry_size@2120,
    # bootconfig_size@2124
    header_size = struct.unpack('<I', d[2096:2100])[0]
    dtb_size = struct.unpack('<I', d[2100:2104])[0]
    vrt_size = struct.unpack('<I', d[2112:2116])[0]
    bootconfig_size = struct.unpack('<I', d[2124:2128])[0]
    PS = page_size
    # The header itself occupies `n` pages (header_size rounded up to page_size), NOT one — this
    # image uses page_size=2048 with header_size=2128, so n=2. Sections follow after n pages.
    n = align(header_size, PS) // PS
    p = align(vendor_ramdisk_size, PS) // PS
    q = align(dtb_size, PS) // PS
    r = align(vrt_size, PS) // PS
    vr_off = n * PS
    vendor_ramdisk = d[vr_off:vr_off + vendor_ramdisk_size]
    bc_off = PS * (n + p + q + r)
    bootconfig = d[bc_off:bc_off + bootconfig_size]
    return vendor_ramdisk, bootconfig


def main():
    cf = sys.argv[1]
    img = os.path.join(cf, 'img')
    kernel, _ = parse_boot(os.path.join(img, 'boot.img'))
    _, init_ramdisk = parse_boot(os.path.join(img, 'init_boot.img'))
    vendor_ramdisk, bootconfig = parse_vendor_boot(os.path.join(img, 'vendor_boot.img'))

    def write(name, data):
        p = os.path.join(cf, name)
        with open(p, 'wb') as f:
            f.write(data)
        print(f"  wrote {name:26s} {len(data)} bytes")

    write('kernel', kernel)
    write('init_boot_ramdisk.cpio.gz', init_ramdisk)
    write('vendor_ramdisk.cpio.gz', vendor_ramdisk)
    write('initramfs.cf.gz', init_ramdisk + vendor_ramdisk)
    # bootconfig section is NUL/newline-terminated key=val text (+ a trailing size/checksum trailer
    # in some images); keep only the printable text lines.
    text = bytes(b for b in bootconfig if b == 0x0a or 0x20 <= b <= 0x7e)
    write('bootconfig.txt', text.rstrip(b'\x00') + (b'\n' if text and not text.endswith(b'\n') else b''))
    print(f"kernel magic {kernel[:4].hex()} (MZ=arm64 Image); vendor_ramdisk {len(vendor_ramdisk)} B")


if __name__ == '__main__':
    main()
