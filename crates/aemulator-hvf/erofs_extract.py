#!/usr/bin/env python3
# erofs_extract.py — minimal read-only EROFS walker over a liblp `super.raw.img`.
#
# Purpose: verify the 64-bit-only Cuttlefish image by extracting the first bytes (ELF header) of
# named binaries inside the read-only `vendor_a` / `system_a` EROFS partitions — without a Linux host
# (no lpunpack/fuse.erofs on macOS). Locates the target logical partition's extent by parsing liblp
# metadata (same as prep-super.sh), parses the EROFS superblock, walks the directory tree, and reads
# each requested file's leading data block.
#
#   python3 erofs_extract.py <super.raw.img> <partition> <path> [<out>] [--head N] [--ls]
#     e.g. python3 erofs_extract.py img/super.raw.img vendor_a /bin/hw/android.hardware.audio.service - --head 64
#          python3 erofs_extract.py img/super.raw.img vendor_a /bin/hw --ls
#
# Handles inode datalayouts FLAT_PLAIN(0) + FLAT_INLINE(2); flags COMPRESSED(1/3) + CHUNK(4) so the
# caller knows a plaintext ELF read isn't possible for that inode.
import os, struct, sys


def lp_partition_extent(fd, super_path, want):
    RESERVED = 4096
    GEO = 4096

    def pread(off, n):
        return os.pread(fd, n, off)

    geo = pread(RESERVED, 4096)
    assert struct.unpack('<I', geo[0:4])[0] == 0x616c4467, "bad LP geometry magic"
    meta_off = RESERVED + GEO * 2
    hdr = pread(meta_off, 256)
    assert struct.unpack('<I', hdr[0:4])[0] == 0x414c5030, "bad LP header magic"
    header_size = struct.unpack('<I', hdr[8:12])[0]

    def desc(o):
        return struct.unpack('<III', hdr[o:o + 12])

    p_off, p_num, p_sz = desc(80)
    e_off, e_num, e_sz = desc(92)
    tables_off = meta_off + header_size
    parts = {}
    for i in range(p_num):
        b = pread(tables_off + p_off + i * p_sz, p_sz)
        name = b[0:36].split(b'\0')[0].decode()
        _, first_extent, num_extents, _ = struct.unpack('<IIII', b[36:52])
        parts[name] = (first_extent, num_extents)
    if want not in parts:
        raise SystemExit(f"partition {want!r} not in super: {sorted(parts)}")
    first_extent, num_extents = parts[want]
    assert num_extents == 1, f"{want} fragmented ({num_extents} extents)"
    b = pread(tables_off + e_off + first_extent * e_sz, e_sz)
    num_sectors, target_type, target_data, target_source = struct.unpack('<QIQI', b[0:24])
    return target_data * 512, num_sectors * 512


class Erofs:
    def __init__(self, fd, base):
        self.fd = fd
        self.base = base
        sb = self.pread(1024, 128)
        magic = struct.unpack('<I', sb[0:4])[0]
        assert magic == 0xE0F5E1E2, f"not EROFS (magic {magic:#x})"
        self.blkszbits = sb[12]
        self.blksz = 1 << self.blkszbits
        self.root_nid = struct.unpack('<H', sb[14:16])[0]
        self.meta_blkaddr = struct.unpack('<I', sb[40:44])[0]
        self.feature_incompat = struct.unpack('<I', sb[80:84])[0]

    def pread(self, off, n):
        return os.pread(self.fd, n, self.base + off)

    def inode(self, nid):
        io = self.meta_blkaddr * self.blksz + nid * 32
        raw = self.pread(io, 64)
        i_format = struct.unpack('<H', raw[0:2])[0]
        extended = (i_format & 1) == 1
        datalayout = (i_format >> 1) & 7
        xattr_icount = struct.unpack('<H', raw[2:4])[0]
        mode = struct.unpack('<H', raw[4:6])[0]
        if extended:
            size = struct.unpack('<Q', raw[8:16])[0]
            raw_blk = struct.unpack('<I', raw[16:20])[0]
            inode_sz = 64
        else:
            size = struct.unpack('<I', raw[8:12])[0]
            raw_blk = struct.unpack('<I', raw[16:20])[0]
            inode_sz = 32
        xattr_sz = (12 + (xattr_icount - 1) * 4) if xattr_icount else 0
        inline_off = io + inode_sz + xattr_sz
        return dict(nid=nid, datalayout=datalayout, mode=mode, size=size,
                    raw_blk=raw_blk, inline_off=inline_off, io=io)

    def read_data(self, ino, length):
        """Read up to `length` bytes of file/dir data. FLAT_PLAIN reads from raw_blk;
        FLAT_INLINE reads full blocks from raw_blk then the inline tail. Returns bytes."""
        dl = ino['datalayout']
        size = min(length, ino['size'])
        if dl == 0:  # FLAT_PLAIN
            return self.pread(ino['raw_blk'] * self.blksz, size)
        if dl == 2:  # FLAT_INLINE
            full = ino['size'] // self.blksz * self.blksz
            out = b''
            if size <= full:
                return self.pread(ino['raw_blk'] * self.blksz, size)
            if full:
                out += self.pread(ino['raw_blk'] * self.blksz, full)
            tail = self.pread(ino['inline_off'], ino['size'] - full)
            out += tail
            return out[:size]
        raise ValueError(f"datalayout {dl} not readable (compressed/chunk)")

    def listdir(self, ino):
        """Yield (name, child_nid, file_type) for a directory inode."""
        assert (ino['mode'] & 0o170000) == 0o040000, "not a directory"
        data = self.read_data(ino, ino['size'])
        entries = []
        blksz = self.blksz
        nblocks = (ino['size'] + blksz - 1) // blksz
        for blk in range(nblocks):
            base = blk * blksz
            block = data[base:base + blksz]
            if len(block) < 12:
                break
            first_nameoff = struct.unpack('<H', block[8:10])[0]
            count = first_nameoff // 12
            for k in range(count):
                de = block[k * 12:(k + 1) * 12]
                if len(de) < 12:
                    break
                nid = struct.unpack('<Q', de[0:8])[0]
                nameoff = struct.unpack('<H', de[8:10])[0]
                ftype = de[10]
                end = struct.unpack('<H', block[(k + 1) * 12 + 8:(k + 1) * 12 + 10])[0] \
                    if k + 1 < count else None
                if end is None:
                    name = block[nameoff:].split(b'\0')[0]
                else:
                    name = block[nameoff:end]
                name = name.rstrip(b'\0')
                if name:
                    entries.append((name.decode('utf-8', 'replace'), nid, ftype))
        return entries

    def resolve(self, path):
        parts = [p for p in path.strip('/').split('/') if p]
        cur = self.inode(self.root_nid)
        for p in parts:
            found = None
            for name, nid, ftype in self.listdir(cur):
                if name == p:
                    found = nid
                    break
            if found is None:
                raise SystemExit(f"path component {p!r} not found in {path!r}")
            cur = self.inode(found)
        return cur


def main():
    args = sys.argv[1:]
    ls = '--ls' in args
    if ls:
        args.remove('--ls')
    head = 64
    if '--head' in args:
        i = args.index('--head')
        head = int(args[i + 1])
        del args[i:i + 2]
    super_path, part, path = args[0], args[1], args[2]
    out = args[3] if len(args) > 3 else None
    fd = os.open(super_path, os.O_RDONLY)
    base, _ = lp_partition_extent(fd, super_path, part)
    ero = Erofs(fd, base)
    ino = ero.resolve(path)
    dl_names = {0: 'FLAT_PLAIN', 1: 'COMPRESSED_FULL', 2: 'FLAT_INLINE', 3: 'COMPRESSED_COMPACT', 4: 'CHUNK'}
    print(f"{part}:{path}  mode={ino['mode']:#o} size={ino['size']} "
          f"datalayout={dl_names.get(ino['datalayout'], ino['datalayout'])}")
    if ls:
        for name, nid, ftype in sorted(ero.listdir(ino)):
            print(f"  [{ftype}] nid={nid} {name}")
        return
    data = ero.read_data(ino, head)
    if data[:4] == b'\x7fELF':
        ei_class = data[4]
        em = struct.unpack('<H', data[18:20])[0]
        names = {0x28: 'ARM (32-bit)', 0xB7: 'AArch64 (64-bit)', 0x3e: 'x86-64', 0x03: 'x86'}
        print(f"  ELF ei_class={'64' if ei_class == 2 else '32'}-bit  "
              f"e_machine={em:#x} -> {names.get(em, 'unknown')}")
    else:
        print(f"  (not an ELF; first bytes {data[:8]!r})")
    if out and out != '-':
        with open(out, 'wb') as f:
            f.write(ero.read_data(ino, ino['size']))
        print(f"  wrote {out}")


if __name__ == '__main__':
    main()
