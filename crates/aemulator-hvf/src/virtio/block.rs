//! virtio-block (device type 2): a single request virtqueue backed by a host file. Each request is
//! a descriptor chain of `[ 16-byte header | data... | 1-byte status ]`; the header carries the
//! request type + starting sector. We service IN (read), OUT (write), FLUSH and GET_ID.

use super::{Mem, VIRTIO_F_VERSION_1, VIRTIO_ID_BLOCK};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

const SECTOR_SIZE: u64 = 512;
const HEADER_LEN: usize = 16;

pub struct Block {
    file: File,
    capacity_sectors: u64,
}

impl Block {
    /// Back the device with `file`, whose current length (rounded down to 512) is the capacity.
    pub fn new(file: File) -> std::io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            capacity_sectors: len / SECTOR_SIZE,
        })
    }

    pub fn device_type(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }

    /// `VIRTIO_BLK_F_FLUSH` (bit 9) is mandatory for correctness here, not optional tuning: without
    /// it the guest assumes there is no volatile write cache and ext4 issues no barriers at all,
    /// while our writes sit in the host page cache. See
    /// [`super::block_composite::CompositeBlock::features`].
    pub fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | (1u64 << 9)
    }

    pub fn num_queues(&self) -> usize {
        1
    }

    /// virtio-blk config: `capacity` (u64 sectors) at offset 0. `len` ∈ {1,2,4,8}.
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
            // Collect the chain so we can identify header / data / status descriptors.
            let descs: Vec<(u64, u32, bool)> = chain
                .map(|d| (d.addr().0, d.len(), d.is_write_only()))
                .collect();
            let written = self.handle_request(mem, &descs).unwrap_or(0);
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

    /// Returns the number of bytes the device wrote back (for `add_used` length).
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

        // Last descriptor is the 1-byte status; everything between is data.
        let status_idx = descs.len() - 1;
        let (saddr, _slen, _) = descs[status_idx];
        let data = &descs[1..status_idx];

        let (status, data_written) = match req_type {
            VIRTIO_BLK_T_IN => self.do_read(mem, sector, data),
            VIRTIO_BLK_T_OUT => (self.do_write(mem, sector, data), 0),
            // `sync_data` (fdatasync), NOT `Write::flush` — the latter is a no-op for `std::fs::File`
            // and never reaches the kernel, so the device used to acknowledge guest barriers without
            // making anything durable. See `block_composite::CompositeBlock::features` for the full
            // failure mode (silently non-crash-consistent images).
            VIRTIO_BLK_T_FLUSH => (
                if self.file.sync_data().is_ok() {
                    VIRTIO_BLK_S_OK
                } else {
                    VIRTIO_BLK_S_IOERR
                },
                0,
            ),
            VIRTIO_BLK_T_GET_ID => self.do_get_id(mem, data),
            _ => (VIRTIO_BLK_S_UNSUPP, 0),
        };

        let _ = mem.write_slice(&[status], GuestAddress(saddr));
        // Used length = device-written data + the status byte.
        Some(data_written + 1)
    }

    fn do_read(&mut self, mem: &Mem, sector: u64, data: &[(u64, u32, bool)]) -> (u8, u32) {
        if self
            .file
            .seek(SeekFrom::Start(sector * SECTOR_SIZE))
            .is_err()
        {
            return (VIRTIO_BLK_S_IOERR, 0);
        }
        let mut total = 0u32;
        for &(addr, len, write_only) in data {
            if !write_only {
                continue;
            }
            let mut buf = vec![0u8; len as usize];
            // Short reads past EOF leave zeros — fine for an unformatted scratch disk.
            let n = self.file.read(&mut buf).unwrap_or(0);
            if n < buf.len() {
                for b in buf.iter_mut().skip(n) {
                    *b = 0;
                }
            }
            if mem.write_slice(&buf, GuestAddress(addr)).is_err() {
                return (VIRTIO_BLK_S_IOERR, total);
            }
            total += len;
        }
        (VIRTIO_BLK_S_OK, total)
    }

    fn do_write(&mut self, mem: &Mem, sector: u64, data: &[(u64, u32, bool)]) -> u8 {
        if self
            .file
            .seek(SeekFrom::Start(sector * SECTOR_SIZE))
            .is_err()
        {
            return VIRTIO_BLK_S_IOERR;
        }
        for &(addr, len, write_only) in data {
            if write_only {
                continue;
            }
            let mut buf = vec![0u8; len as usize];
            if mem.read_slice(&mut buf, GuestAddress(addr)).is_err()
                || self.file.write_all(&buf).is_err()
            {
                return VIRTIO_BLK_S_IOERR;
            }
        }
        VIRTIO_BLK_S_OK
    }

    fn do_get_id(&mut self, mem: &Mem, data: &[(u64, u32, bool)]) -> (u8, u32) {
        let id = b"aemulator-vda\0\0\0\0\0\0\0"; // up to 20 bytes
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
}
