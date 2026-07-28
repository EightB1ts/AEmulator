//! virtio-input (device type 18): injects host keyboard + pointer events into the guest as a Linux
//! `evdev` device. The default configuration is a **combined tablet**: an absolute pointer
//! (`EV_ABS` with `ABS_X`/`ABS_Y` over a 0..32767 range, plus `BTN_LEFT`/`BTN_TOUCH`) *and* a full
//! keyboard (`EV_KEY` codes 0..255). A window layer calls [`Input::queue_event`] /
//! [`Input::flush_to_eventq`] (surfaced on the transport as `inject_input_events`) to push events.
//!
//! Config space is the standard select/subsel-multiplexed `virtio_input_config` (virtio spec
//! §5.8.3): the guest writes `select`+`subsel` then reads back `size` + a `payload` describing the
//! device name, ids, supported event bitmaps, and abs-axis info. Two virtqueues: **eventq** (0,
//! host→guest events) and **statusq** (1, guest→host status such as LED state — drained + acked).
//! Reference: crosvm `devices/src/virtio/input` (not copied).

use super::{Mem, VIRTIO_F_VERSION_1};
use std::collections::VecDeque;
use virtio_queue::{Queue, QueueT};
use vm_memory::{Bytes, GuestAddress};

pub const VIRTIO_ID_INPUT: u32 = 18;

pub const EVENTQ: u16 = 0;
pub const STATUSQ: u16 = 1;

// Linux input event types / codes (subset).
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_TOUCH: u16 = 0x14a;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
/// INPUT_PROP_DIRECT (input-event-codes.h, bit 1) — device is a DIRECT input whose absolute
/// coordinates map 1:1 onto the display (a touchscreen), as opposed to INPUT_PROP_POINTER (bit 0, an
/// indirect touchpad). Android's InputReader classifies an `ABS_X`/`ABS_Y` + `BTN_TOUCH` device with
/// this prop as a TOUCH_SCREEN bound to the internal display, so an absolute bottom→top drag is a
/// real directional screen swipe (which dismisses the insecure keyguard). This requires the internal
/// display to expose a DisplayViewport with physical properties — satisfied by the virtio-gpu EDID.
pub const INPUT_PROP_DIRECT: u16 = 0x01;

/// Full absolute range advertised for ABS_X / ABS_Y (a window maps its pixel coords into this).
pub const ABS_MAX: u32 = 32767;

// virtio_input config select values (linux/virtio_input.h).
const VIRTIO_INPUT_CFG_UNSET: u8 = 0x00;
const VIRTIO_INPUT_CFG_ID_NAME: u8 = 0x01;
const VIRTIO_INPUT_CFG_ID_SERIAL: u8 = 0x02;
const VIRTIO_INPUT_CFG_ID_DEVIDS: u8 = 0x03;
const VIRTIO_INPUT_CFG_PROP_BITS: u8 = 0x10;
const VIRTIO_INPUT_CFG_EV_BITS: u8 = 0x11;
const VIRTIO_INPUT_CFG_ABS_INFO: u8 = 0x12;

/// One virtio_input_event on the wire: type_(u16) code(u16) value(i32), little-endian = 8 bytes.
pub const EVENT_SIZE: usize = 8;

fn encode_event(type_: u16, code: u16, value: i32) -> [u8; EVENT_SIZE] {
    let mut b = [0u8; EVENT_SIZE];
    b[0..2].copy_from_slice(&type_.to_le_bytes());
    b[2..4].copy_from_slice(&code.to_le_bytes());
    b[4..8].copy_from_slice(&value.to_le_bytes());
    b
}

/// Which class of device this instance advertises.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// Absolute pointer + keyboard in one node (the default the window layer drives).
    Tablet,
    /// Keyboard only.
    Keyboard,
}

pub struct Input {
    kind: InputKind,
    select: u8,
    subsel: u8,
    /// Encoded events awaiting a guest-posted eventq buffer.
    pending: VecDeque<[u8; EVENT_SIZE]>,
}

impl Input {
    pub fn new_tablet() -> Self {
        Self::new(InputKind::Tablet)
    }

    pub fn new_keyboard() -> Self {
        Self::new(InputKind::Keyboard)
    }

    fn new(kind: InputKind) -> Self {
        Self {
            kind,
            select: 0,
            subsel: 0,
            pending: VecDeque::new(),
        }
    }

    pub fn device_type(&self) -> u32 {
        VIRTIO_ID_INPUT
    }

    pub fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }

    pub fn num_queues(&self) -> usize {
        2
    }

    fn name(&self) -> &'static str {
        match self.kind {
            InputKind::Tablet => "aemulator-tablet",
            InputKind::Keyboard => "aemulator-keyboard",
        }
    }

    fn has_abs(&self) -> bool {
        self.kind == InputKind::Tablet
    }

    /// Config reads are multiplexed by the last-written (select, subsel). Byte 0 = select, 1 =
    /// subsel, 2 = size, 3..8 reserved, 8.. = payload.
    pub fn config_read(&self, off: u64, len: usize) -> u64 {
        let img = self.build_config();
        read_le(&img, off as usize, len)
    }

    pub fn config_write(&mut self, off: u64, val: u64, len: usize) {
        // Only select (0) and subsel (1) are writable.
        for i in 0..len {
            let byte = ((val >> (8 * i)) & 0xff) as u8;
            match off as usize + i {
                0 => self.select = byte,
                1 => self.subsel = byte,
                _ => {}
            }
        }
    }

    /// Build the 8-byte header + 128-byte payload config image for the current (select, subsel).
    fn build_config(&self) -> Vec<u8> {
        let mut img = vec![0u8; 8 + 128];
        img[0] = self.select;
        img[1] = self.subsel;
        let payload = self.config_payload();
        let size = payload.len().min(128);
        img[2] = size as u8;
        img[8..8 + size].copy_from_slice(&payload[..size]);
        img
    }

    /// The payload bytes for the current (select, subsel), per virtio-input's config multiplexing.
    fn config_payload(&self) -> Vec<u8> {
        match self.select {
            VIRTIO_INPUT_CFG_ID_NAME => self.name().as_bytes().to_vec(),
            VIRTIO_INPUT_CFG_ID_SERIAL => b"aemulator0".to_vec(),
            VIRTIO_INPUT_CFG_ID_DEVIDS => {
                // bustype, vendor, product, version (u16 LE each). BUS_VIRTUAL = 0x06.
                let mut v = Vec::new();
                v.extend_from_slice(&0x06u16.to_le_bytes()); // bustype
                v.extend_from_slice(&0x1af4u16.to_le_bytes()); // vendor (Red Hat / virtio)
                v.extend_from_slice(&0x0001u16.to_le_bytes()); // product
                v.extend_from_slice(&0x0001u16.to_le_bytes()); // version
                v
            }
            // INPUT_PROP_DIRECT (bit 1) — declare a real touchscreen. Android's InputReader binds an
            // ABS_X/Y + BTN_TOUCH device carrying this prop to the internal display's DisplayViewport
            // (now valid because the virtio-gpu reports a physical size via EDID), so an absolute
            // bottom→top drag produces a directional screen swipe that dismisses the swipe keyguard.
            // (Under the old INPUT_PROP_POINTER the device was an indirect touchpad using RELATIVE
            // gestures, so an absolute drag never became a screen swipe.)
            VIRTIO_INPUT_CFG_PROP_BITS if self.has_abs() => bitmap_from_bits(&[INPUT_PROP_DIRECT]),
            VIRTIO_INPUT_CFG_PROP_BITS => Vec::new(),
            VIRTIO_INPUT_CFG_EV_BITS => self.ev_bits(self.subsel),
            VIRTIO_INPUT_CFG_ABS_INFO => self.abs_info(self.subsel),
            VIRTIO_INPUT_CFG_UNSET => Vec::new(),
            _ => Vec::new(),
        }
    }

    /// Bitmap of supported codes for event type `subsel`. Empty (size 0) ⇒ type unsupported, which
    /// is how the guest learns which EV_* types this device emits.
    fn ev_bits(&self, ev_type: u8) -> Vec<u8> {
        match ev_type as u16 {
            EV_KEY => {
                // Keyboard keys 0..255 + pointer buttons (BTN_LEFT, BTN_TOUCH on the tablet).
                let mut bits: Vec<u16> = (0u16..256).collect();
                if self.has_abs() {
                    bits.push(BTN_LEFT);
                    bits.push(BTN_TOUCH);
                }
                bitmap_from_bits(&bits)
            }
            EV_ABS if self.has_abs() => bitmap_from_bits(&[ABS_X, ABS_Y]),
            _ => Vec::new(),
        }
    }

    /// virtio_input_absinfo { min, max, fuzz, flat } (u32 LE each) for abs axis `subsel`.
    fn abs_info(&self, axis: u8) -> Vec<u8> {
        if !self.has_abs() {
            return Vec::new();
        }
        match axis as u16 {
            ABS_X | ABS_Y => {
                let mut v = Vec::new();
                v.extend_from_slice(&0u32.to_le_bytes()); // min
                v.extend_from_slice(&ABS_MAX.to_le_bytes()); // max
                v.extend_from_slice(&0u32.to_le_bytes()); // fuzz
                v.extend_from_slice(&0u32.to_le_bytes()); // flat
                v
            }
            _ => Vec::new(),
        }
    }

    /// Queue one raw event for delivery (no SYN appended). Call [`flush_to_eventq`] to deliver.
    pub fn queue_event(&mut self, type_: u16, code: u16, value: i32) {
        self.pending.push_back(encode_event(type_, code, value));
    }

    /// Queue a batch of events followed by a single `EV_SYN`/`SYN_REPORT` frame terminator, the
    /// grouping evdev consumers expect after a coherent set of changes.
    pub fn queue_frame(&mut self, events: &[(u16, u16, i32)]) {
        for &(t, c, v) in events {
            self.queue_event(t, c, v);
        }
        self.queue_event(EV_SYN, SYN_REPORT, 0);
    }

    pub fn process_queue(&mut self, idx: u16, mem: &Mem, queue: &mut Queue) -> bool {
        match idx {
            EVENTQ => self.flush_to_eventq(mem, queue),
            STATUSQ => self.drain_statusq(mem, queue),
            _ => false,
        }
    }

    /// Fill guest-posted eventq buffers with queued events (one 8-byte event per descriptor chain).
    /// Returns whether any buffer was completed (→ raise the device SPI).
    pub fn flush_to_eventq(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while !self.pending.is_empty() {
            let Some(chain) = queue.pop_descriptor_chain(mem) else {
                break;
            };
            let head = chain.head_index();
            // First device-writable descriptor receives the event.
            let target = chain
                .into_iter()
                .find(|d| d.is_write_only() && d.len() as usize >= EVENT_SIZE)
                .map(|d| d.addr().0);
            let mut written = 0u32;
            if let (Some(addr), Some(ev)) = (target, self.pending.front().copied()) {
                if mem.write_slice(&ev, GuestAddress(addr)).is_ok() {
                    self.pending.pop_front();
                    written = EVENT_SIZE as u32;
                }
            }
            let _ = queue.add_used(mem, head, written);
            used = true;
        }
        used
    }

    /// Drain the status queue (guest→host: LED / repeat-rate updates). We ignore the content and
    /// just return the buffers so the ring keeps flowing.
    fn drain_statusq(&mut self, mem: &Mem, queue: &mut Queue) -> bool {
        let mut used = false;
        while let Some(chain) = queue.pop_descriptor_chain(mem) {
            let head = chain.head_index();
            let _ = queue.add_used(mem, head, 0);
            used = true;
        }
        used
    }
}

/// Build a little-endian bit array with `set_indices` bits set (min-length trimmed of trailing 0s).
fn bitmap_from_bits(set_indices: &[u16]) -> Vec<u8> {
    let max = set_indices.iter().copied().max().unwrap_or(0) as usize;
    let mut bytes = vec![0u8; max / 8 + 1];
    for &idx in set_indices {
        bytes[idx as usize / 8] |= 1 << (idx % 8);
    }
    bytes
}

/// Read a little-endian value of `len` ∈ {1,2,4,8} bytes at `off`, zero-extended, bounds-safe.
fn read_le(b: &[u8], off: usize, len: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..len {
        if let Some(byte) = b.get(off + i) {
            v |= (*byte as u64) << (8 * i);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_encoding_is_le_type_code_value() {
        let b = encode_event(EV_ABS, ABS_X, 12345);
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), EV_ABS);
        assert_eq!(u16::from_le_bytes([b[2], b[3]]), ABS_X);
        assert_eq!(i32::from_le_bytes([b[4], b[5], b[6], b[7]]), 12345);
        // Negative values round-trip (relative motion / signed axes).
        let n = encode_event(EV_ABS, ABS_Y, -7);
        assert_eq!(i32::from_le_bytes([n[4], n[5], n[6], n[7]]), -7);
    }

    #[test]
    fn config_name_select_returns_device_name() {
        let d = Input::new_tablet();
        // select = ID_NAME.
        let mut d = d;
        d.config_write(0, VIRTIO_INPUT_CFG_ID_NAME as u64, 1);
        let size = d.config_read(2, 1) as usize;
        assert_eq!(size, "aemulator-tablet".len());
        let mut got = Vec::new();
        for i in 0..size {
            got.push(d.config_read(8 + i as u64, 1) as u8);
        }
        assert_eq!(&got, b"aemulator-tablet");
    }

    #[test]
    fn tablet_advertises_ev_key_and_ev_abs_but_keyboard_has_no_abs() {
        let tablet = Input::new_tablet();
        assert!(!tablet.ev_bits(EV_KEY as u8).is_empty());
        assert!(!tablet.ev_bits(EV_ABS as u8).is_empty());
        // BTN_LEFT bit set in the EV_KEY bitmap.
        let key_bits = tablet.ev_bits(EV_KEY as u8);
        assert_ne!(key_bits[BTN_LEFT as usize / 8] & (1 << (BTN_LEFT % 8)), 0);
        // abs axis info present + correct max.
        let ax = tablet.abs_info(ABS_X as u8);
        assert_eq!(read_le(&ax, 4, 4), ABS_MAX as u64);

        let kbd = Input::new_keyboard();
        assert!(!kbd.ev_bits(EV_KEY as u8).is_empty());
        assert!(kbd.ev_bits(EV_ABS as u8).is_empty());
        assert!(kbd.abs_info(ABS_X as u8).is_empty());
    }

    #[test]
    fn tablet_advertises_input_prop_direct() {
        let mut d = Input::new_tablet();
        d.config_write(0, VIRTIO_INPUT_CFG_PROP_BITS as u64, 1);
        let size = d.config_read(2, 1) as usize;
        let mut prop = Vec::new();
        for i in 0..size {
            prop.push(d.config_read(8 + i as u64, 1) as u8);
        }
        // INPUT_PROP_DIRECT is bit 1 → byte 0, mask 0x02 (touchscreen, not touchpad).
        assert!(!prop.is_empty());
        assert_ne!(prop[0] & (1 << INPUT_PROP_DIRECT), 0, "must advertise INPUT_PROP_DIRECT");
        assert_eq!(prop[0] & 0x01, 0, "must NOT advertise INPUT_PROP_POINTER (bit 0)");
    }

    #[test]
    fn queue_frame_appends_syn_terminator() {
        let mut d = Input::new_tablet();
        d.queue_frame(&[(EV_ABS, ABS_X, 100), (EV_ABS, ABS_Y, 200)]);
        // 2 events + 1 SYN.
        assert_eq!(d.pending.len(), 3);
        let last = *d.pending.back().unwrap();
        assert_eq!(u16::from_le_bytes([last[0], last[1]]), EV_SYN);
    }
}
