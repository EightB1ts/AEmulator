//! Guest RAM: a single contiguous host mmap, mapped 1:1 into the guest at the DRAM base IPA.
//!
//! We use `vm_memory::GuestMemoryMmap` for the host allocation (it `mmap`s a page-aligned region
//! and gives us `Bytes`/loader helpers for free), then hand its raw host pointer to
//! `hv_vm_map` so HVF stage-2 maps `[DRAM_BASE, DRAM_BASE+size)` → that host range as RWX.

use crate::*;
use std::os::raw::c_void;
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryMmap};

/// Guest-physical base of DRAM. Mirrors crosvm/QEMU-virt (`AARCH64_PHYS_MEM_START`).
pub const DRAM_BASE: u64 = 0x8000_0000;

/// A single-region guest RAM plus the bookkeeping the VMM needs.
pub struct GuestRam {
    pub mem: GuestMemoryMmap,
    pub base: u64,
    pub size: usize,
}

impl GuestRam {
    /// Allocate `size` bytes of host RAM (page-aligned via mmap) for a guest DRAM region rooted at
    /// `DRAM_BASE`. `size` must be a multiple of the host granule (16 KiB on Apple Silicon).
    pub fn new(size: usize) -> Result<Self, String> {
        let page = host_page_size();
        if !size.is_multiple_of(page) {
            return Err(format!(
                "guest RAM size {size:#x} is not a multiple of the host page size {page:#x}"
            ));
        }
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(DRAM_BASE), size)])
            .map_err(|e| format!("GuestMemoryMmap::from_ranges failed: {e:?}"))?;
        Ok(Self {
            mem,
            base: DRAM_BASE,
            size,
        })
    }

    /// Raw host pointer to the start of the region (page-aligned, from mmap).
    pub fn host_ptr(&self) -> Result<*mut u8, String> {
        self.mem
            .get_host_address(GuestAddress(self.base))
            .map_err(|e| format!("get_host_address({:#x}) failed: {e:?}", self.base))
    }

    /// Map the whole region into the guest's stage-2 address space as RWX.
    pub fn map_into_vm(&self) -> Result<(), String> {
        let host = self.host_ptr()?;
        let flags = HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC;
        // Safety: `host` points at `self.size` bytes of live mmap'd memory owned by `self.mem`,
        // which outlives the mapping (we never unmap+free while the VM runs).
        let rc = unsafe { hv_vm_map(host as *mut c_void, self.base, self.size, flags) };
        if rc != HV_SUCCESS {
            return Err(format!(
                "hv_vm_map(host={host:p}, ipa={:#x}, size={:#x}) -> {} ({:#010x})",
                self.base,
                self.size,
                hv_return_name(rc),
                rc as u32
            ));
        }
        Ok(())
    }

    /// Unmap the region from the guest (best-effort teardown).
    pub fn unmap_from_vm(&self) {
        let _ = unsafe { hv_vm_unmap(self.base, self.size) };
    }
}
