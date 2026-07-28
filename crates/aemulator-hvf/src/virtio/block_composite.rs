//! virtio-block **GPT composite disk** backend (Android/Cuttlefish disk presentation).
//!
//! Where [`super::block::Block`] backs one virtio-blk device with one raw file, this backend
//! presents **one** virtio-blk device whose contents are a synthetic **GUID Partition Table**: a
//! protective MBR + primary GPT header + a 128-entry partition array (with the required Cuttlefish
//! `PARTNAME` labels: `super`, `boot_a`, `init_boot_a`, `vbmeta_a`, `vbmeta_system_a`,
//! `vbmeta_vendor_dlkm_a`, `metadata`, `userdata`, `misc`, …) + a backup GPT at the tail. Each GPT
//! partition entry maps to a backing **component file** at a 4 KiB-aligned LBA offset; a guest block
//! read/write at byte `pos` is routed to the component file that owns that offset (GPT header/array
//! regions are served from in-memory buffers).
//!
//! This is the disk format `crosvm` builds for Cuttlefish (see `crosvm/disk/src/composite.rs` +
//! `gpt.rs`); Android's `ueventd` surfaces `/dev/block/by-name/<label>` symlinks from the GPT
//! partition names, which is exactly what first-stage init's `BlockDevInitializer` waits for.
//!
//! The layout arithmetic (GPT_BEGINNING_SIZE, 4 KiB partition alignment, footer padding, mixed-
//! endian GUIDs, CRC32 over header + entry array) mirrors crosvm byte-for-byte so a stock Linux GPT
//! parser accepts the primary header.

use super::{Mem, VIRTIO_F_VERSION_1, VIRTIO_ID_BLOCK};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

/// `VIRTIO_BLK_F_FLUSH` — "device supports a cache-flush command". See [`CompositeBlock::features`]
/// for why this must be advertised (without it the guest never issues barriers at all).
const VIRTIO_BLK_F_FLUSH: u32 = 9;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const SECTOR_SIZE: u64 = 512;
const HEADER_LEN: usize = 16;

// --- GPT geometry (matches crosvm/disk/src/gpt.rs) ---
const GPT_HEADER_SIZE: u32 = 92;
const GPT_NUM_PARTITIONS: u32 = 128;
const GPT_PARTITION_ENTRY_SIZE: u32 = 128;
/// Everything before the first partition: protective MBR + primary header + entry array + padding.
const GPT_BEGINNING_SIZE: u64 = SECTOR_SIZE * 40; // 20480
/// Everything after the last partition: backup entry array + backup header.
const GPT_END_SIZE: u64 = SECTOR_SIZE * 33; // 16896
/// Keep every partition 4 KiB-aligned (crosvm's `PARTITION_SIZE_SHIFT`).
const PARTITION_ALIGN: u64 = 1 << 12; // 4096
const HEADER_PADDING_LEN: usize = SECTOR_SIZE as usize - GPT_HEADER_SIZE as usize; // 420
/// Zero padding between the last partition-array entry and the first partition (see crosvm).
const PARTITION_ALIGNMENT_SIZE: usize = GPT_BEGINNING_SIZE as usize
    - 2 * SECTOR_SIZE as usize
    - GPT_NUM_PARTITIONS as usize * GPT_PARTITION_ENTRY_SIZE as usize; // 3072

/// Linux-filesystem partition-type GUID `0FC63DAF-8483-4772-8E79-3D69D8477DE4` in GPT's mixed-endian
/// on-disk form (first three fields little-endian, trailing 8 bytes as-is). ueventd resolves by-name
/// from the partition NAME, not the type GUID, so a single generic type for every entry is fine.
const LINUX_FS_TYPE_GUID: [u8; 16] = [
    0xAF, 0xC6, 0xFC, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];
/// A fixed, non-zero disk GUID (any stable value; the parser only needs it present).
const DISK_GUID: [u8; 16] = [
    0xAE, 0x30, 0x11, 0x50, 0x9C, 0x0B, 0x4A, 0x77, 0xB1, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x00,
];

/// One partition to place in the composite disk. Owned so it can be moved across the `--display`
/// worker-thread boundary before the backing files are opened.
#[derive(Clone, Debug)]
pub struct PartitionSpec {
    /// GPT partition NAME → `/dev/block/by-name/<label>` (already slot-suffixed, e.g. `boot_a`).
    pub label: String,
    /// Backing raw image file.
    pub path: PathBuf,
    /// Whether guest writes are allowed to land in the backing file (userdata/misc/metadata).
    pub writable: bool,
}

/// A placed component: its byte range in the composite disk + the open backing file.
struct Component {
    /// Byte offset of this partition within the composite (guest-visible) disk.
    offset: u64,
    /// 4 KiB-aligned length reserved in the composite disk (>= file_len).
    length: u64,
    file: File,
    /// Actual backing-file length; bytes in `[file_len, length)` read back as zeros.
    file_len: u64,
    writable: bool,
}

pub struct CompositeBlock {
    /// In-memory image of `[0, GPT_BEGINNING_SIZE)` — protective MBR + primary GPT.
    beginning: Vec<u8>,
    /// In-memory image of `[footer_offset, disk_size)` — padding + backup entry array + backup GPT.
    footer: Vec<u8>,
    footer_offset: u64,
    disk_size: u64,
    capacity_sectors: u64,
    components: Vec<Component>,
    /// Debug: count served requests so we can log the first few (gated on AEMU_CDISK_DEBUG).
    req_count: u64,
}

impl CompositeBlock {
    /// Build the composite disk from `specs` (in order). Opens each backing file (read-write when
    /// `writable`, else read-only) and synthesizes the GPT.
    pub fn new(specs: &[PartitionSpec]) -> std::io::Result<Self> {
        use std::fs::OpenOptions;

        // 1. Place components at 4 KiB-aligned offsets starting after the primary GPT region.
        let mut components: Vec<Component> = Vec::with_capacity(specs.len());
        let mut next_offset = GPT_BEGINNING_SIZE;
        let mut entries = vec![0u8; (GPT_NUM_PARTITIONS * GPT_PARTITION_ENTRY_SIZE) as usize];

        for (i, spec) in specs.iter().enumerate() {
            let file = OpenOptions::new()
                .read(true)
                .write(spec.writable)
                .open(&spec.path)?;
            let file_len = file.metadata()?.len();
            let length = file_len
                .next_multiple_of(PARTITION_ALIGN)
                .max(PARTITION_ALIGN);
            let offset = next_offset;

            // Write this partition's GPT entry.
            let entry = build_entry(&spec.label, offset, length, i as u8);
            let base = i * GPT_PARTITION_ENTRY_SIZE as usize;
            entries[base..base + GPT_PARTITION_ENTRY_SIZE as usize].copy_from_slice(&entry);

            components.push(Component {
                offset,
                length,
                file,
                file_len,
                writable: spec.writable,
            });
            next_offset += length;
        }

        // 2. Footer geometry (backup GPT at the very end; padding keeps it 4 KiB-aligned start).
        let footer_padding = GPT_END_SIZE.next_multiple_of(PARTITION_ALIGN) - GPT_END_SIZE;
        let footer_offset = next_offset;
        let secondary_table_offset = footer_offset + footer_padding;
        let disk_size = secondary_table_offset + GPT_END_SIZE;

        // 3. CRC32 of the partition-entry array (shared by both headers).
        let entries_crc = crc32(&entries);

        // 4. Beginning buffer: protective MBR + primary header + entry array + alignment padding.
        let mut beginning = Vec::with_capacity(GPT_BEGINNING_SIZE as usize);
        write_protective_mbr(&mut beginning, disk_size);
        write_gpt_header(
            &mut beginning,
            entries_crc,
            secondary_table_offset,
            disk_size,
            false,
        );
        beginning.extend(std::iter::repeat_n(0u8, HEADER_PADDING_LEN));
        beginning.extend_from_slice(&entries);
        beginning.extend(std::iter::repeat_n(0u8, PARTITION_ALIGNMENT_SIZE));
        debug_assert_eq!(beginning.len(), GPT_BEGINNING_SIZE as usize);

        // 5. Footer buffer: padding + backup entry array + backup header.
        let mut footer = Vec::with_capacity((footer_padding + GPT_END_SIZE) as usize);
        footer.extend(std::iter::repeat_n(0u8, footer_padding as usize));
        footer.extend_from_slice(&entries);
        write_gpt_header(
            &mut footer,
            entries_crc,
            secondary_table_offset,
            disk_size,
            true,
        );
        footer.extend(std::iter::repeat_n(0u8, HEADER_PADDING_LEN));
        debug_assert_eq!(footer.len() as u64, disk_size - footer_offset);

        Ok(Self {
            beginning,
            footer,
            footer_offset,
            disk_size,
            capacity_sectors: disk_size / SECTOR_SIZE,
            components,
            req_count: 0,
        })
    }

    /// Human-readable summary of the placed partitions (for the boot banner).
    pub fn describe(&self, specs: &[PartitionSpec]) -> Vec<String> {
        self.components
            .iter()
            .zip(specs.iter())
            .map(|(c, s)| {
                format!(
                    "{:<22} LBA {:>8}..{:<8} ({} MiB img{}) → {}",
                    s.label,
                    c.offset / SECTOR_SIZE,
                    (c.offset + c.length) / SECTOR_SIZE - 1,
                    c.file_len / (1024 * 1024),
                    if c.writable { ", rw" } else { "" },
                    s.path.file_name().and_then(|x| x.to_str()).unwrap_or("?"),
                )
            })
            .collect()
    }

    pub fn total_size(&self) -> u64 {
        self.disk_size
    }

    pub fn device_type(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }

    /// Advertise `VIRTIO_BLK_F_FLUSH` alongside `VIRTIO_F_VERSION_1`.
    ///
    /// **This bit is a data-integrity requirement, not a performance knob.** Per the virtio spec,
    /// when `VIRTIO_BLK_F_FLUSH` is NOT negotiated the driver must assume the device has no volatile
    /// write cache — every completed write is already durable. Linux takes that literally: ext4 stops
    /// issuing barriers entirely, because there is nothing to barrier against. Our writes, meanwhile,
    /// land in the HOST page cache. So the journal's ordering guarantee (commit block only after the
    /// data it describes is stable) evaporated: on any host-side kill the image could hold a journal
    /// commit whose data blocks never reached the platter, which surfaces in the guest as
    ///
    /// ```text
    /// EXT4-fs error (device vda9): ext4_lookup: inode #NNN: iget: checksum invalid
    /// EXT4-fs error (device vda9): deleted inode referenced
    /// ```
    ///
    /// — i.e. the exact corruption that repeatedly destroyed instance userdata. Advertising FLUSH
    /// makes the guest issue real barriers, which [`Self::flush_all`] turns into `fdatasync`. Writes
    /// get slower; the filesystem becomes crash-consistent, so even SIGKILL degrades to a journal
    /// replay instead of an unrecoverable image.
    pub fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | (1u64 << VIRTIO_BLK_F_FLUSH)
    }

    /// `fdatasync` every writable component. Returns the first error encountered.
    ///
    /// `sync_data` (fdatasync) rather than `sync_all` (fsync): the file contents must be durable, but
    /// these are fixed-size preallocated images whose metadata never changes, so syncing the inode on
    /// every guest barrier is pure cost. Read-only components can't be dirty, so they are skipped.
    ///
    /// NOTE: `Write::flush()` — which this code used to call — is a **no-op** for `std::fs::File`.
    /// It flushes Rust-side buffering (of which `File` has none) and never reaches the kernel, so the
    /// device was acknowledging every guest barrier without making anything durable.
    pub fn flush_all(&mut self) -> std::io::Result<()> {
        let mut first_err = None;
        for c in &mut self.components {
            if !c.writable {
                continue;
            }
            if let Err(e) = c.file.sync_data() {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn num_queues(&self) -> usize {
        1
    }

    /// virtio-blk config: `capacity` (u64 sectors) at offset 0.
    pub fn config_read(&self, off: u64, len: usize) -> u64 {
        let bytes = self.capacity_sectors.to_le_bytes();
        let mut v = 0u64;
        for i in 0..len {
            let idx = off as usize + i;
            if idx < bytes.len() {
                v |= (bytes[idx] as u64) << (8 * i);
            }
        }
        v
    }

    pub fn config_write(&mut self, _off: u64, _val: u64, _len: usize) {}

    pub fn process_queue(&mut self, _idx: u16, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let descs: Vec<(u64, u32, bool)> = chain
                .map(|d| (d.addr().0, d.len(), d.is_write_only()))
                .collect();
            let written = self.handle_request(mem, &descs).unwrap_or(0);
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

    fn handle_request(&mut self, mem: &Mem, descs: &[(u64, u32, bool)]) -> Option<u32> {
        if descs.len() < 2 {
            return None;
        }
        let (haddr, hlen, _) = descs[0];
        if (hlen as usize) < HEADER_LEN {
            return None;
        }
        let mut header = [0u8; HEADER_LEN];
        mem.read_slice(&mut header, GuestAddress(haddr)).ok()?;
        let req_type = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());

        self.req_count += 1;
        if std::env::var_os("AEMU_CDISK_DEBUG").is_some() && self.req_count <= 40 {
            let bytes: u32 = descs[1..descs.len() - 1].iter().map(|d| d.1).sum();
            eprintln!(
                "[cdisk] req#{} type={} sector={} ({} bytes)",
                self.req_count, req_type, sector, bytes
            );
        }

        let status_idx = descs.len() - 1;
        let (saddr, _slen, _) = descs[status_idx];
        let data = &descs[1..status_idx];

        let (status, data_written) = match req_type {
            VIRTIO_BLK_T_IN => self.do_read(mem, sector, data),
            VIRTIO_BLK_T_OUT => (self.do_write(mem, sector, data), 0),
            VIRTIO_BLK_T_FLUSH => {
                // Report the truth. Answering OK on a failed fsync tells ext4 its journal commit is
                // stable when it isn't, which is strictly worse than surfacing an I/O error.
                match self.flush_all() {
                    Ok(()) => (VIRTIO_BLK_S_OK, 0),
                    Err(e) => {
                        eprintln!("[cdisk] FLUSH failed: {e}");
                        (VIRTIO_BLK_S_IOERR, 0)
                    }
                }
            }
            VIRTIO_BLK_T_GET_ID => self.do_get_id(mem, data),
            _ => (VIRTIO_BLK_S_UNSUPP, 0),
        };

        let _ = mem.write_slice(&[status], GuestAddress(saddr));
        Some(data_written + 1)
    }

    fn do_read(&mut self, mem: &Mem, sector: u64, data: &[(u64, u32, bool)]) -> (u8, u32) {
        let mut pos = sector * SECTOR_SIZE;
        let mut total = 0u32;
        for &(addr, len, write_only) in data {
            if !write_only {
                continue;
            }
            let mut buf = vec![0u8; len as usize];
            if self.read_at(pos, &mut buf).is_err() {
                return (VIRTIO_BLK_S_IOERR, total);
            }
            if mem.write_slice(&buf, GuestAddress(addr)).is_err() {
                return (VIRTIO_BLK_S_IOERR, total);
            }
            pos += len as u64;
            total += len;
        }
        (VIRTIO_BLK_S_OK, total)
    }

    fn do_write(&mut self, mem: &Mem, sector: u64, data: &[(u64, u32, bool)]) -> u8 {
        let mut pos = sector * SECTOR_SIZE;
        for &(addr, len, write_only) in data {
            if write_only {
                continue;
            }
            let mut buf = vec![0u8; len as usize];
            if mem.read_slice(&mut buf, GuestAddress(addr)).is_err() {
                return VIRTIO_BLK_S_IOERR;
            }
            if self.write_at(pos, &buf).is_err() {
                return VIRTIO_BLK_S_IOERR;
            }
            pos += len as u64;
        }
        VIRTIO_BLK_S_OK
    }

    fn do_get_id(&mut self, mem: &Mem, data: &[(u64, u32, bool)]) -> (u8, u32) {
        let id = b"aemulator-cdisk\0\0\0\0\0"; // up to 20 bytes
        let mut total = 0u32;
        for &(addr, len, write_only) in data {
            if !write_only {
                continue;
            }
            let n = (len as usize).min(id.len());
            if mem.write_slice(&id[..n], GuestAddress(addr)).is_err() {
                return (VIRTIO_BLK_S_IOERR, total);
            }
            total += n as u32;
            break;
        }
        (VIRTIO_BLK_S_OK, total)
    }

    /// Fill `buf` from the composite disk starting at byte `pos`, walking region by region
    /// (beginning GPT / component files / footer GPT), zero-filling holes and past-EOF tails.
    fn read_at(&mut self, mut pos: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let want = buf.len() - done;
            if pos >= self.disk_size {
                for b in buf[done..].iter_mut() {
                    *b = 0;
                }
                break;
            }
            if pos < GPT_BEGINNING_SIZE {
                let n = want.min((GPT_BEGINNING_SIZE - pos) as usize);
                let s = pos as usize;
                buf[done..done + n].copy_from_slice(&self.beginning[s..s + n]);
                pos += n as u64;
                done += n;
            } else if pos >= self.footer_offset {
                let s = (pos - self.footer_offset) as usize;
                let n = want.min((self.disk_size - pos) as usize);
                buf[done..done + n].copy_from_slice(&self.footer[s..s + n]);
                pos += n as u64;
                done += n;
            } else {
                // Inside a component (or its 4 KiB alignment tail).
                let comp = self
                    .components
                    .iter_mut()
                    .find(|c| pos >= c.offset && pos < c.offset + c.length);
                let Some(comp) = comp else {
                    // Should not happen (components are contiguous); zero-fill defensively.
                    let n = want.min((self.footer_offset - pos) as usize);
                    for b in buf[done..done + n].iter_mut() {
                        *b = 0;
                    }
                    pos += n as u64;
                    done += n;
                    continue;
                };
                let region_end = comp.offset + comp.length;
                let n = want.min((region_end - pos) as usize);
                let file_pos = pos - comp.offset;
                let dst = &mut buf[done..done + n];
                if file_pos < comp.file_len {
                    let from_file = (comp.file_len - file_pos).min(n as u64) as usize;
                    comp.file.seek(SeekFrom::Start(file_pos))?;
                    let mut read = 0usize;
                    while read < from_file {
                        let r = comp.file.read(&mut dst[read..from_file])?;
                        if r == 0 {
                            break;
                        }
                        read += r;
                    }
                    for b in dst[read..].iter_mut() {
                        *b = 0;
                    }
                } else {
                    for b in dst.iter_mut() {
                        *b = 0;
                    }
                }
                pos += n as u64;
                done += n;
            }
        }
        Ok(())
    }

    /// Route a guest write to the backing component file (only within the file's real length, and
    /// only for writable partitions). Writes to the GPT regions / alignment gaps are dropped.
    fn write_at(&mut self, mut pos: u64, buf: &[u8]) -> std::io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let want = buf.len() - done;
            if pos >= self.disk_size {
                break;
            }
            if pos < GPT_BEGINNING_SIZE {
                let n = want.min((GPT_BEGINNING_SIZE - pos) as usize);
                pos += n as u64;
                done += n;
            } else if pos >= self.footer_offset {
                let n = want.min((self.disk_size - pos) as usize);
                pos += n as u64;
                done += n;
            } else {
                let comp = self
                    .components
                    .iter_mut()
                    .find(|c| pos >= c.offset && pos < c.offset + c.length);
                let Some(comp) = comp else {
                    let n = want.min((self.footer_offset - pos) as usize);
                    pos += n as u64;
                    done += n;
                    continue;
                };
                let region_end = comp.offset + comp.length;
                let n = want.min((region_end - pos) as usize);
                let file_pos = pos - comp.offset;
                if comp.writable && file_pos < comp.file_len {
                    let to_file = (comp.file_len - file_pos).min(n as u64) as usize;
                    comp.file.seek(SeekFrom::Start(file_pos))?;
                    comp.file.write_all(&buf[done..done + to_file])?;
                }
                pos += n as u64;
                done += n;
            }
        }
        Ok(())
    }
}

/// Build a 128-byte GPT partition entry.
fn build_entry(label: &str, offset: u64, length: u64, idx: u8) -> [u8; 128] {
    let mut e = [0u8; 128];
    e[0..16].copy_from_slice(&LINUX_FS_TYPE_GUID);
    // Unique partition GUID — deterministic, non-zero, unique per index.
    let unique = [
        0xAE, 0x33, 0x77, 0x01, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB,
        idx,
    ];
    e[16..32].copy_from_slice(&unique);
    let first_lba = offset / SECTOR_SIZE;
    let last_lba = (offset + length) / SECTOR_SIZE - 1;
    e[32..40].copy_from_slice(&first_lba.to_le_bytes());
    e[40..48].copy_from_slice(&last_lba.to_le_bytes());
    // attributes (48..56) = 0.
    // partition name: UTF-16LE, up to 36 code units, at bytes 56..128.
    let mut ni = 56;
    for u in label.encode_utf16().take(36) {
        e[ni..ni + 2].copy_from_slice(&u.to_le_bytes());
        ni += 2;
    }
    e
}

/// Write a protective MBR (one whole-disk 0xEE partition) into `out` (512 bytes).
fn write_protective_mbr(out: &mut Vec<u8>, disk_size: u64) {
    let start = out.len();
    out.extend(std::iter::repeat_n(0u8, 446)); // bootstrap
    out.push(0x00); // status
    out.extend_from_slice(&[0, 0, 0]); // CHS begin
    out.push(0xEE); // type: GPT protective
    out.extend_from_slice(&[0, 0, 0]); // CHS end
    out.extend_from_slice(&1u32.to_le_bytes()); // first LBA
    let num_sectors = ((disk_size / SECTOR_SIZE).saturating_sub(1)).min(0xFFFF_FFFF) as u32;
    out.extend_from_slice(&num_sectors.to_le_bytes());
    out.extend(std::iter::repeat_n(0u8, 16 * 3)); // three empty entries
    out.extend_from_slice(&[0x55, 0xAA]); // boot signature
    debug_assert_eq!(out.len() - start, SECTOR_SIZE as usize);
}

/// Write a 92-byte GPT header (primary or backup) into `out`.
fn write_gpt_header(
    out: &mut Vec<u8>,
    entries_crc: u32,
    secondary_table_offset: u64,
    disk_size: u64,
    secondary: bool,
) {
    let primary_lba: u64 = 1;
    let secondary_lba: u64 = disk_size / SECTOR_SIZE - 1;
    debug_assert_eq!(
        secondary_lba,
        (secondary_table_offset + GPT_END_SIZE) / SECTOR_SIZE - 1
    );

    let mut h = [0u8; GPT_HEADER_SIZE as usize];
    h[0..8].copy_from_slice(b"EFI PART");
    h[8..12].copy_from_slice(&[0, 0, 1, 0]); // revision 1.0
    h[12..16].copy_from_slice(&GPT_HEADER_SIZE.to_le_bytes());
    // h[16..20] header_crc32 — filled after CRC.
    // h[20..24] reserved = 0.
    let current = if secondary {
        secondary_lba
    } else {
        primary_lba
    };
    let backup = if secondary {
        primary_lba
    } else {
        secondary_lba
    };
    h[24..32].copy_from_slice(&current.to_le_bytes());
    h[32..40].copy_from_slice(&backup.to_le_bytes());
    h[40..48].copy_from_slice(&(GPT_BEGINNING_SIZE / SECTOR_SIZE).to_le_bytes()); // first usable
    h[48..56].copy_from_slice(&(secondary_table_offset / SECTOR_SIZE - 1).to_le_bytes()); // last usable
    h[56..72].copy_from_slice(&DISK_GUID);
    h[72..80].copy_from_slice(&2u64.to_le_bytes()); // partition_entries_lba (crosvm: always 2)
    h[80..84].copy_from_slice(&GPT_NUM_PARTITIONS.to_le_bytes());
    h[84..88].copy_from_slice(&GPT_PARTITION_ENTRY_SIZE.to_le_bytes());
    h[88..92].copy_from_slice(&entries_crc.to_le_bytes());

    let crc = crc32(&h);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&h);
}

/// Sync writable images on teardown, so a graceful VMM exit always leaves a clean disk.
///
/// Placed on `Drop` rather than in a `stop()`/`quit` handler deliberately: the device is dropped on
/// EVERY normal exit path (control-plane `quit`, `--max-iters` completion, windowed close, an error
/// return), and one hook that cannot be forgotten beats N call sites that can. Failures are logged,
/// not panicked — `Drop` must not unwind, and by this point the guest is already gone.
impl Drop for CompositeBlock {
    fn drop(&mut self) {
        if let Err(e) = self.flush_all() {
            eprintln!("[cdisk] final sync on teardown failed: {e}");
        }
    }
}

/// CRC-32 (IEEE 802.3, reflected, poly 0xEDB88320) — the checksum GPT headers/entries use.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Known-answer CRC32: "123456789" → 0xCBF43926 (standard CRC-32/ISO-HDLC test vector).
    #[test]
    fn crc32_known_answer() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Data-integrity contract, not a style preference: without `VIRTIO_BLK_F_FLUSH` the guest's
    /// ext4 stops issuing barriers (it is told there is no volatile cache) while our writes sit in
    /// the host page cache — which corrupted instance userdata repeatedly. If this assertion ever
    /// fails, read `CompositeBlock::features` before "fixing" the test.
    #[test]
    fn advertises_flush_so_the_guest_issues_barriers() {
        let specs = vec![PartitionSpec {
            label: "userdata".into(),
            path: tmp_img("flushfeat", 4096),
            writable: true,
        }];
        let cd = CompositeBlock::new(&specs).unwrap();
        let feats = cd.features();
        assert_ne!(
            feats & (1 << VIRTIO_BLK_F_FLUSH),
            0,
            "VIRTIO_BLK_F_FLUSH must be advertised or the guest never sends barriers"
        );
        assert_ne!(
            feats & VIRTIO_F_VERSION_1,
            0,
            "VERSION_1 must stay advertised"
        );
    }

    /// A guest barrier must reach the kernel. `flush_all` is the fdatasync path; a no-op flush (the
    /// old `Write::flush`) would also "succeed", so this asserts reachability + writability handling
    /// rather than durability itself (unobservable in-process).
    #[test]
    fn flush_all_succeeds_over_mixed_writable_components() {
        let specs = vec![
            PartitionSpec {
                label: "super".into(),
                path: tmp_img("fl-ro", 4096),
                writable: false,
            },
            PartitionSpec {
                label: "userdata".into(),
                path: tmp_img("fl-rw", 4096),
                writable: true,
            },
        ];
        let mut cd = CompositeBlock::new(&specs).unwrap();
        // Read-only components are skipped; syncing them would fail on an O_RDONLY fd.
        cd.flush_all()
            .expect("flush over mixed rw/ro components must succeed");
    }

    fn tmp_img(name: &str, bytes: u64) -> PathBuf {
        let p = std::env::temp_dir().join(format!("aemu-cdisk-test-{name}"));
        let mut f = File::create(&p).unwrap();
        // Write a recognizable header then set length.
        f.write_all(format!("IMG:{name}").as_bytes()).unwrap();
        f.set_len(bytes).unwrap();
        p
    }

    #[test]
    fn gpt_layout_and_routing() {
        let specs = vec![
            PartitionSpec {
                label: "super".into(),
                path: tmp_img("super", 8192),
                writable: false,
            },
            PartitionSpec {
                label: "boot_a".into(),
                path: tmp_img("boot", 5000), // unaligned → 8192 aligned slot
                writable: false,
            },
            PartitionSpec {
                label: "userdata".into(),
                path: tmp_img("userdata", 4096),
                writable: true,
            },
        ];
        let mut cd = CompositeBlock::new(&specs).unwrap();

        // --- Protective MBR signature + GPT header magic in the beginning region. ---
        let mut mbr = [0u8; 512];
        cd.read_at(0, &mut mbr).unwrap();
        assert_eq!(&mbr[510..512], &[0x55, 0xAA], "protective MBR signature");
        assert_eq!(mbr[450], 0xEE, "MBR partition type = GPT protective");

        let mut hdr = [0u8; 92];
        cd.read_at(SECTOR_SIZE, &mut hdr).unwrap();
        assert_eq!(&hdr[0..8], b"EFI PART", "primary GPT signature");
        // Header CRC must validate (zero the crc field, recompute, compare).
        let stored_crc = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
        let mut h2 = hdr;
        h2[16..20].copy_from_slice(&[0, 0, 0, 0]);
        assert_eq!(
            crc32(&h2),
            stored_crc,
            "primary header CRC32 self-consistent"
        );

        // Partition-array CRC in header must match the array actually written at LBA 2.
        let entries_crc = u32::from_le_bytes(hdr[88..92].try_into().unwrap());
        let mut arr = vec![0u8; (GPT_NUM_PARTITIONS * GPT_PARTITION_ENTRY_SIZE) as usize];
        cd.read_at(2 * SECTOR_SIZE, &mut arr).unwrap();
        assert_eq!(crc32(&arr), entries_crc, "entry-array CRC32 matches header");

        // --- First entry names "super" (UTF-16LE at byte 56) and starts at LBA 40. ---
        let name: Vec<u16> = (0..5)
            .map(|i| u16::from_le_bytes(arr[56 + i * 2..58 + i * 2].try_into().unwrap()))
            .collect();
        assert_eq!(String::from_utf16(&name).unwrap(), "super");
        let first_lba = u64::from_le_bytes(arr[32..40].try_into().unwrap());
        assert_eq!(
            first_lba,
            GPT_BEGINNING_SIZE / SECTOR_SIZE,
            "super first LBA"
        );

        // --- Backup GPT header at the very last sector. ---
        let mut bhdr = [0u8; 92];
        cd.read_at(cd.disk_size - SECTOR_SIZE, &mut bhdr).unwrap();
        assert_eq!(&bhdr[0..8], b"EFI PART", "backup GPT signature");

        // --- Content routing: reading the super partition's first bytes returns its file header. ---
        let super_off = GPT_BEGINNING_SIZE;
        let mut sb = [0u8; 9];
        cd.read_at(super_off, &mut sb).unwrap();
        assert_eq!(&sb, b"IMG:super", "super partition routes to backing file");

        // boot_a placed right after super's aligned slot (8192 aligned).
        let boot_off = GPT_BEGINNING_SIZE + 8192;
        let mut bb = [0u8; 8];
        cd.read_at(boot_off, &mut bb).unwrap();
        assert_eq!(&bb, b"IMG:boot", "boot_a routes to its backing file");

        // Reading past a file's real length inside its aligned slot returns zeros (5000→8192).
        let mut tail = [0xffu8; 16];
        cd.read_at(boot_off + 5000, &mut tail).unwrap();
        assert!(
            tail.iter().all(|&b| b == 0),
            "alignment tail is zero-filled"
        );
    }
}
