//! An ARM PL011 UART with enough behaviour for both the guest's polled `earlycon` **and** the full
//! interrupt-driven `amba-pl011` tty driver — so a login/rescue shell on `ttyAMA0` is bidirectional.
//!
//! TX: a write to UARTDR emits the byte to host stdout (the earlycon path spins on UARTFR waiting
//! for the TX FIFO to drain, which we always report as ready).
//!
//! RX: host-typed bytes are pushed into an RX FIFO by the stdin reader thread (`machine.rs`). When
//! the guest has unmasked the receive interrupt (UARTIMSC.RXIM) we raise the PL011 SPI; its ISR then
//! reads UARTMIS, drains UARTDR until UARTFR.RXFE, and clears via UARTICR. This is what makes the
//! shell actually receive keystrokes (the tty layer is interrupt-driven, not polled).
//!
//! The device is wired to SPI 1 (INTID 33) in the FDT (`boot.rs`), matching [`PL011_INTID`].

use std::collections::VecDeque;
use std::io::Write;

/// QEMU-virt PL011 base — matches the `earlycon`/`console=ttyAMA0` address a stock kernel expects.
pub const PL011_BASE: u64 = 0x0900_0000;
pub const PL011_SIZE: u64 = 0x1000;

/// Absolute GIC INTID of the PL011 (FDT declares it as SPI 1 → 32 + 1).
pub const PL011_INTID: u32 = 33;

const UARTDR: u64 = 0x000; // Data register
const UARTFR: u64 = 0x018; // Flag register
const UARTIMSC: u64 = 0x038; // Interrupt mask set/clear
const UARTRIS: u64 = 0x03c; // Raw interrupt status
const UARTMIS: u64 = 0x040; // Masked interrupt status
const UARTICR: u64 = 0x044; // Interrupt clear

// AMBA PrimeCell identification registers (0xFE0..=0xFFC, one byte each, step 4). The `amba` bus
// reads these to match a device to a driver — WITHOUT them the `amba-pl011` driver never binds, so
// `ttyAMA0` never registers as a console and the kernel reports "unable to open an initial console"
// (the polled earlycon still works, since it does raw MMIO with no ID check). Byte values are the
// canonical ARM PL011 PrimeCell id (periphid 0x00041011 under mask 0x000fffff; cellid 0xb105f00d),
// identical to QEMU's `pl011_id_arm`.
const UARTID_BASE: u64 = 0xFE0;
const PL011_ID: [u8; 8] = [0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

// UARTFR flag bits (PL011 TRM).
const FR_RXFE: u32 = 1 << 4; // Receive FIFO empty
const FR_TXFF: u32 = 1 << 5; // Transmit FIFO full
const FR_TXFE: u32 = 1 << 7; // Transmit FIFO empty

// Interrupt bits (shared by RIS / MIS / IMSC / ICR).
const INT_RX: u32 = 1 << 4; // Receive interrupt
const INT_RT: u32 = 1 << 6; // Receive timeout interrupt

/// State for the emulated UART.
pub struct Pl011 {
    /// Total bytes the guest has written — a cheap "did earlycon actually run?" signal.
    pub bytes_out: u64,
    /// Host-typed bytes awaiting the guest's UARTDR reads.
    rx_fifo: VecDeque<u8>,
    /// UARTIMSC — which interrupts the guest has unmasked.
    imsc: u32,
}

impl Default for Pl011 {
    fn default() -> Self {
        Self::new()
    }
}

impl Pl011 {
    pub fn new() -> Self {
        Self {
            bytes_out: 0,
            rx_fifo: VecDeque::new(),
            imsc: 0,
        }
    }

    /// Is `ipa` inside this device's MMIO window?
    #[inline]
    pub fn contains(ipa: u64) -> bool {
        ipa >= PL011_BASE && ipa < PL011_BASE + PL011_SIZE
    }

    /// Raw interrupt status, derived from FIFO state (RX + RX-timeout assert while data waits).
    fn ris(&self) -> u32 {
        if self.rx_fifo.is_empty() {
            0
        } else {
            INT_RX | INT_RT
        }
    }

    /// Handle a guest MMIO read. Returns the value the guest register should receive.
    pub fn read(&mut self, ipa: u64, _len: usize) -> u64 {
        match ipa - PL011_BASE {
            UARTDR => self.rx_fifo.pop_front().unwrap_or(0) as u64,
            UARTFR => {
                let mut fr = FR_TXFE; // TX FIFO always empty (never full).
                if self.rx_fifo.is_empty() {
                    fr |= FR_RXFE;
                }
                (fr & !FR_TXFF) as u64
            }
            UARTIMSC => self.imsc as u64,
            UARTRIS => self.ris() as u64,
            UARTMIS => (self.ris() & self.imsc) as u64,
            off if (UARTID_BASE..UARTID_BASE + 0x20).contains(&off) && off % 4 == 0 => {
                PL011_ID[((off - UARTID_BASE) / 4) as usize] as u64
            }
            _ => 0,
        }
    }

    /// Handle a guest MMIO write.
    pub fn write(&mut self, ipa: u64, val: u64, _len: usize) {
        match ipa - PL011_BASE {
            UARTDR => {
                let byte = (val & 0xff) as u8;
                self.bytes_out += 1;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&[byte]);
                let _ = out.flush();
            }
            UARTIMSC => self.imsc = val as u32,
            UARTICR => { /* interrupts are level-derived from the FIFO; nothing latched to clear */ }
            _ => {}
        }
    }

    /// Push host-typed bytes into the RX FIFO. Returns `true` if the guest has unmasked the receive
    /// interrupt (→ the caller raises [`PL011_INTID`] so the tty ISR runs).
    pub fn push_rx(&mut self, bytes: &[u8]) -> bool {
        self.rx_fifo.extend(bytes.iter().copied());
        (self.imsc & INT_RX) != 0
    }
}
