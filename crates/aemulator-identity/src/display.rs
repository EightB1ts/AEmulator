//! EDID 1.4 base-block emission for the profile's panel.
//!
//! The guest's DRM connector learns the panel's *mode* and its *physical size* from exactly one
//! blob: the 128-byte EDID base block the virtio-gpu device answers `VIRTIO_GPU_CMD_GET_EDID`
//! with. Android then turns that into `DisplayMetrics` (`xdpi`/`ydpi`), into the internal display's
//! `DisplayViewport`, and — because a `DisplayViewport` with a physical size is the precondition
//! for `INPUT_PROP_DIRECT` to associate — into whether the touchscreen works at all.
//!
//! So this module lives in a **workspace member**: it is the highest-risk bit-twiddling in the
//! display stack and its tests must run in CI, not in the workspace-excluded VMM crate.
//!
//! ## Why the timing is computed, not tabulated
//!
//! The previous emitter (in the VMM) took `(width, height, dpi)` but embedded a frozen VESA DMT
//! 1024×768@60 detailed timing: only the millimetre bytes responded to its arguments, and the
//! established-timings bit for 1024×768@60 was set unconditionally. Any other resolution produced
//! a block that *contradicted itself* — a 1080×2400 scanout advertising a 1024×768 preferred mode.
//! [`build_edid`] instead derives a real detailed timing from `(width, height, refresh)` with
//! [`cvt_reduced_blanking`], so the block is correct for every mode this device can scan out.
//!
//! ## Invariants this module guarantees (each pinned by a test)
//!
//! * The 128 bytes sum to 0 mod 256 (byte 127 is the checksum).
//! * The physical-size bytes — 21/22 (cm) and the detailed timing's 12..15 (mm) — are **never
//!   zero**, whatever the caller passes. A zero physical size is what breaks `INPUT_PROP_DIRECT`.
//! * The established-timings bytes (35..38) are zero: 1024×768@60 is not this panel's native mode.
//! * The range-limits descriptor covers the emitted timing, so the block cannot contradict itself.
//! * No string in the block names AEmulator or any real product.

/// EDID horizontal cell granularity, in pixels. A detailed timing's horizontal values are
/// expressible at 1-pixel granularity, but CVT rounds the active width down to a multiple of 8
/// (VESA CVT 1.2 `CELL_GRAN`), and the guest kernel compares the preferred mode against the
/// scanout size — so a width that is not a multiple of 8 is a coherence failure, not a rounding
/// detail. [`crate::DeviceProfile::validate`]'s EDID round-trip rule is what catches it.
const CELL_GRAN: u32 = 8;

/// CVT reduced-blanking horizontal blanking period, in pixels (VESA CVT 1.2: `RB_H_BLANK`). Fixed
/// by the standard — reduced blanking exists precisely to make this a constant.
const RB_H_BLANK: u32 = 160;
/// CVT reduced-blanking horizontal front porch (`RB_H_BLANK` = 48 front + 32 sync + 80 back).
const RB_H_FRONT_PORCH: u32 = 48;
/// CVT reduced-blanking horizontal sync pulse width, in pixels.
const RB_H_SYNC_WIDTH: u32 = 32;
/// CVT reduced-blanking minimum vertical blanking period, in microseconds (`RB_MIN_V_BLANK`).
const RB_MIN_V_BLANK_US: f64 = 460.0;
/// CVT reduced-blanking vertical front porch, in lines (`RB_V_FPORCH`).
const RB_V_FRONT_PORCH: u32 = 3;
/// CVT reduced-blanking minimum vertical back porch, in lines (`RB_MIN_V_BPORCH`).
const RB_MIN_V_BACK_PORCH: u32 = 6;

/// The pixel clock's unit in an EDID detailed timing descriptor: 10 kHz, in a 16-bit field.
const PCLK_UNIT_HZ: u64 = 10_000;

/// Panel-envelope floor for the display range-limits descriptor: minimum vertical field rate (Hz).
/// A 48 Hz floor is what a modern variable-refresh phone panel reports.
const RANGE_MIN_VRATE_HZ: u32 = 48;
/// Panel-envelope floor for the maximum vertical field rate (Hz) — covers a 120 Hz panel plus the
/// customary 1 Hz of headroom, so the descriptor stays valid if `refresh_hz` is raised later.
const RANGE_MAX_VRATE_HZ: u32 = 121;
/// Panel-envelope floor for the minimum horizontal line rate (kHz).
const RANGE_MIN_HRATE_KHZ: u32 = 30;
/// Panel-envelope floor for the maximum horizontal line rate (kHz). 1080×2400@60 needs ~148.1 kHz.
const RANGE_MAX_HRATE_KHZ: u32 = 200;
/// Panel-envelope floor for the maximum pixel clock (MHz). 1080×2400@60 needs 183.69 MHz.
const RANGE_MAX_PCLK_MHZ: u32 = 250;

/// The monitor-name descriptor's text. Deliberately **neutral**: this string is readable from the
/// guest (`/sys/class/drm/*/edid`, `edid-decode`, the kernel's connector log line), so it must name
/// neither AEmulator (the previous emitter wrote `"aemulator"` here — a literal product-name leak)
/// nor a real panel model number (a fabricated model code is a fresh, checkable inconsistency).
/// Must stay clear of every substring in `aemulator-detection`'s blocklist.
const PANEL_NAME: &str = "AMOLED Panel";

/// EDID manufacturer id, as three 5-bit letters (`A` = 1). `DSI` names the panel *bus* an internal
/// phone display sits on and claims no registered PNP vendor — the previous emitter used `AEM`,
/// i.e. "AEmulator", in a field the guest can read.
const MANUFACTURER_LETTERS: [u8; 3] = [b'D', b'S', b'I'];

/// A CVT reduced-blanking detailed timing: active/blanking geometry in pixels and lines, plus the
/// pixel clock already quantized to the EDID descriptor's 10 kHz grid (so every value here is
/// exactly what the guest will decode back out — no field is "the ideal we rounded away from").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CvtTiming {
    /// Horizontal active pixels (the caller's width, floored to `CELL_GRAN`, the 8-pixel CVT cell granularity).
    pub h_active: u32,
    /// Horizontal blanking, in pixels. Always 160 (`RB_H_BLANK`) for reduced blanking.
    pub h_blank: u32,
    /// Horizontal front porch — the EDID descriptor's "horizontal sync offset".
    pub h_front_porch: u32,
    /// Horizontal sync pulse width, in pixels.
    pub h_sync_width: u32,
    /// Vertical active lines.
    pub v_active: u32,
    /// Vertical blanking, in lines (front porch + sync + back porch).
    pub v_blank: u32,
    /// Vertical front porch — the EDID descriptor's "vertical sync offset", in lines.
    pub v_front_porch: u32,
    /// Vertical sync pulse width, in lines.
    pub v_sync_width: u32,
    /// Pixel clock in 10 kHz units, i.e. the EDID detailed timing's bytes 0..2.
    pub pixel_clock_10khz: u32,
}

impl CvtTiming {
    /// Total horizontal pixels per line (active + blanking).
    pub fn h_total(&self) -> u32 {
        self.h_active + self.h_blank
    }

    /// Total lines per frame (active + blanking).
    pub fn v_total(&self) -> u32 {
        self.v_active + self.v_blank
    }

    /// The pixel clock in Hz, on the EDID 10 kHz grid.
    pub fn pixel_clock_hz(&self) -> u64 {
        self.pixel_clock_10khz as u64 * PCLK_UNIT_HZ
    }

    /// Horizontal line rate in Hz — `pixel clock / h_total`. The range-limits descriptor's
    /// horizontal window must contain this.
    pub fn line_rate_hz(&self) -> f64 {
        self.pixel_clock_hz() as f64 / self.h_total() as f64
    }

    /// Vertical field rate in Hz as the guest computes it — `pixel clock / (h_total × v_total)`.
    /// Differs from the requested refresh by the 10 kHz clock quantization only (<0.01 Hz at
    /// 1080×2400@60), which is why the round-trip test can assert ±1 Hz.
    pub fn refresh_hz(&self) -> f64 {
        self.pixel_clock_hz() as f64 / (self.h_total() as f64 * self.v_total() as f64)
    }
}

/// Compute a CVT **reduced-blanking** (VESA CVT 1.2 "RB") detailed timing for
/// `width_px × height_px` at `refresh_hz`.
///
/// Reduced blanking is the right family here: the panel is a fixed-timing internal display driven
/// by a virtual scanout, so the CRT-era blanking margins of the general CVT formula buy nothing and
/// only inflate the pixel clock. The derivation, verbatim from the standard:
///
/// ```text
/// H_TOTAL       = H_ACTIVE + RB_H_BLANK(160)
/// H_PERIOD_EST  = ((1e6 / refresh) - RB_MIN_V_BLANK(460 µs)) / V_ACTIVE          [µs]
/// V_BLANK       = max(floor(RB_MIN_V_BLANK / H_PERIOD_EST) + 1,
///                     RB_V_FPORCH(3) + V_SYNC + RB_MIN_V_BPORCH(6))              [lines]
/// V_TOTAL       = V_ACTIVE + V_BLANK
/// PIXEL_CLOCK   = refresh × H_TOTAL × V_TOTAL
/// ```
///
/// Worked examples (both pinned by tests):
///
/// | mode | h_total | v_blank | v_total | pixel clock |
/// |---|---|---|---|---|
/// | 1080×2400@60 | 1240 | 69 | 2469 | 183.69 MHz |
/// | 1024×768@60  | 1184 | 22 |  790 |  56.12 MHz |
///
/// **One deliberate deviation from the standard:** CVT quantizes the pixel clock to a 0.25 MHz
/// `CLOCK_STEP`, but an EDID detailed timing stores it on a 10 kHz grid. Rounding to the finer grid
/// the descriptor can actually express keeps the decoded field rate within 0.002 Hz of the request
/// (0.25 MHz truncation would cost 0.06 Hz at 1080×2400) — and the value the guest computes from
/// the emitted bytes is the only one that matters.
pub fn cvt_reduced_blanking(width_px: u32, height_px: u32, refresh_hz: u32) -> CvtTiming {
    let h_active = (width_px.max(CELL_GRAN) / CELL_GRAN) * CELL_GRAN;
    let v_active = height_px.max(1);
    let refresh = refresh_hz.max(1);
    let v_sync_width = cvt_v_sync_width(h_active, v_active);

    // Minimum blanking the standard's porch structure requires, in lines.
    let min_v_blank = RB_V_FRONT_PORCH + v_sync_width + RB_MIN_V_BACK_PORCH;
    let h_period_est = ((1e6 / refresh as f64) - RB_MIN_V_BLANK_US) / v_active as f64;
    // A refresh so high that the 460 µs blanking floor exceeds the whole frame period leaves no
    // estimate to work from; fall back to the porch minimum rather than dividing by a non-positive
    // period. (Unreachable for any panel this device presents — a guard, not a policy.)
    let v_blank = if h_period_est > 0.0 {
        ((RB_MIN_V_BLANK_US / h_period_est).floor() as u32 + 1).max(min_v_blank)
    } else {
        min_v_blank
    };

    let h_total = h_active + RB_H_BLANK;
    let v_total = v_active + v_blank;
    let pclk_hz = refresh as u64 * h_total as u64 * v_total as u64;
    // Round to the descriptor's 10 kHz grid; clamp into the 16-bit field (655.35 MHz), which no
    // mode a phone panel presents comes near.
    let pixel_clock_10khz = ((pclk_hz + PCLK_UNIT_HZ / 2) / PCLK_UNIT_HZ).clamp(1, 0xFFFF) as u32;

    CvtTiming {
        h_active,
        h_blank: RB_H_BLANK,
        h_front_porch: RB_H_FRONT_PORCH,
        h_sync_width: RB_H_SYNC_WIDTH,
        v_active,
        v_blank,
        v_front_porch: RB_V_FRONT_PORCH,
        v_sync_width,
        pixel_clock_10khz,
    }
}

/// The vertical sync pulse width, in lines, that CVT assigns to an aspect ratio (VESA CVT 1.2
/// table: 4:3 → 4, 16:9 → 5, 16:10 → 6, 5:4 → 7, 15:9 → 7).
///
/// A portrait phone panel is in none of those rows — CVT is a monitor standard and does not
/// contemplate a 9:20 display — so the fallback here is **4 lines**, not the standard's 10. The
/// choice is cosmetic by construction: `V_SYNC` only enters the timing through `RB_MIN_V_BLANK`
/// (3 + V_SYNC + 6 = 13 or 19 lines, both far below the 69 lines the 460 µs floor already demands),
/// so it moves lines between the sync pulse and the back porch and changes no other value. 4 is the
/// pulse width real DSI/eDP panel modes report, so it is the less surprising number in a decode.
fn cvt_v_sync_width(h_active: u32, v_active: u32) -> u32 {
    let (w, h) = (h_active as u64, v_active as u64);
    if w * 3 == h * 4 {
        4 // 4:3
    } else if w * 9 == h * 16 {
        5 // 16:9
    } else if w * 10 == h * 16 {
        6 // 16:10
    } else if w * 4 == h * 5 || w * 9 == h * 15 {
        7 // 5:4 and 15:9
    } else {
        4
    }
}

/// Physical extent, in millimetres, of `pixels` at `dpi` — the dpi-only path (the VMM's Linux
/// default scanout has a density constant but no panel diagonal). Truncates, so the advertised
/// active area is never larger than the real one, matching
/// [`DisplayProfile::physical_mm`](crate::DisplayProfile::physical_mm).
pub fn mm_from_dpi(pixels: u32, dpi: u32) -> u32 {
    if dpi == 0 {
        return 0;
    }
    ((pixels as u64 * 254) / (dpi as u64 * 10)) as u32
}

/// Build a 128-byte EDID 1.4 base block for a `width_px × height_px @ refresh_hz` panel whose
/// active area measures `width_mm × height_mm`.
///
/// The block carries one detailed timing (DTD-1, the preferred/native mode, derived by
/// [`cvt_reduced_blanking`]), a monitor-name descriptor, a display range-limits descriptor sized to
/// contain the emitted timing, and a dummy descriptor. `width_mm`/`height_mm` are clamped into
/// `1..=4095`: a **zero physical size is never emitted**, because the guest's `DisplayViewport`
/// — and therefore `INPUT_PROP_DIRECT` touchscreen association — depends on it being non-zero.
pub fn build_edid(
    width_px: u32,
    height_px: u32,
    refresh_hz: u32,
    width_mm: u32,
    height_mm: u32,
) -> [u8; 128] {
    let timing = cvt_reduced_blanking(width_px, height_px, refresh_hz);
    // Physical size: clamp into the descriptor's 12-bit field AND away from zero.
    let w_mm = width_mm.clamp(1, 4095);
    let h_mm = height_mm.clamp(1, 4095);
    // Bytes 21/22 are whole centimetres (0 = "undefined", so round up to at least 1).
    let w_cm = ((w_mm + 5) / 10).clamp(1, 255) as u8;
    let h_cm = ((h_mm + 5) / 10).clamp(1, 255) as u8;

    let mut e = [0u8; 128];
    // Header magic (bytes 0..8).
    e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // Manufacturer id: three 5-bit letters, big-endian in bytes 8..10.
    let man = MANUFACTURER_LETTERS.iter().fold(0u16, |acc, letter| {
        (acc << 5) | ((*letter - b'A' + 1) as u16)
    });
    e[8] = (man >> 8) as u8;
    e[9] = (man & 0xff) as u8;
    e[10..12].copy_from_slice(&0x0001u16.to_le_bytes()); // product code
    e[12..16].copy_from_slice(&0x0000_0001u32.to_le_bytes()); // serial
    e[16] = 1; // week of manufacture
    e[17] = 34; // year = 1990 + 34 = 2024
    e[18] = 1; // EDID version 1
    e[19] = 4; // EDID revision 4
    e[20] = 0x80; // video input: digital
    e[21] = w_cm; // maximum horizontal image size (cm)
    e[22] = h_cm; // maximum vertical image size (cm)
    e[23] = 0x78; // display gamma 2.2
    e[24] = 0x02; // feature support: DTD-1 is the preferred (native) timing
                  // Chromaticity — standard sRGB-ish coordinates.
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // Established timings (35..38) — ALL ZERO. Byte 36 bit 3 is literally "this panel does
    // 1024x768@60"; the previous emitter set it unconditionally, so every block it produced claimed
    // a VESA DMT mode regardless of what DTD-1 said.
    e[35] = 0x00;
    e[36] = 0x00;
    e[37] = 0x00;
    // Standard timings 1..8 unused (0x0101 = "unused" in both bytes).
    for i in 0..8 {
        e[38 + i * 2] = 0x01;
        e[38 + i * 2 + 1] = 0x01;
    }
    // Descriptor 1 (bytes 54..72): the detailed timing — the preferred, native mode.
    e[54..72].copy_from_slice(&detailed_timing(&timing, w_mm, h_mm));
    // Descriptor 2 (bytes 72..90): monitor name (tag 0xFC), 0x0A-terminated and space-padded.
    e[72..77].copy_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
    let mut name = [0x20u8; 13];
    let text = PANEL_NAME.as_bytes();
    let n = text.len().min(12);
    name[..n].copy_from_slice(&text[..n]);
    name[n] = 0x0A;
    e[77..90].copy_from_slice(&name);
    // Descriptor 3 (bytes 90..108): display range limits (tag 0xFD), sized to contain DTD-1.
    e[90..108].copy_from_slice(&range_limits(&timing));
    // Descriptor 4 (bytes 108..126): dummy (tag 0x10).
    e[108..111].copy_from_slice(&[0x00, 0x00, 0x00]);
    e[111] = 0x10;
    e[126] = 0x00; // extension flag: no extension blocks
                   // Byte 127 is the checksum: the 128 bytes must sum to 0 mod 256.
    let sum: u32 = e[..127].iter().map(|b| *b as u32).sum();
    e[127] = ((256 - (sum % 256)) % 256) as u8;
    e
}

/// Encode `timing` (plus the physical size in mm) as an 18-byte EDID detailed timing descriptor.
fn detailed_timing(timing: &CvtTiming, width_mm: u32, height_mm: u32) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[0..2].copy_from_slice(&(timing.pixel_clock_10khz as u16).to_le_bytes());
    d[2] = (timing.h_active & 0xFF) as u8;
    d[3] = (timing.h_blank & 0xFF) as u8;
    d[4] = ((((timing.h_active >> 8) & 0x0F) << 4) | ((timing.h_blank >> 8) & 0x0F)) as u8;
    d[5] = (timing.v_active & 0xFF) as u8;
    d[6] = (timing.v_blank & 0xFF) as u8;
    d[7] = ((((timing.v_active >> 8) & 0x0F) << 4) | ((timing.v_blank >> 8) & 0x0F)) as u8;
    d[8] = (timing.h_front_porch & 0xFF) as u8;
    d[9] = (timing.h_sync_width & 0xFF) as u8;
    d[10] = (((timing.v_front_porch & 0x0F) << 4) | (timing.v_sync_width & 0x0F)) as u8;
    // Upper bits: hsync offset 9:8, hsync width 9:8, vsync offset 5:4, vsync width 5:4.
    d[11] = ((((timing.h_front_porch >> 8) & 0x03) << 6)
        | (((timing.h_sync_width >> 8) & 0x03) << 4)
        | (((timing.v_front_porch >> 4) & 0x03) << 2)
        | ((timing.v_sync_width >> 4) & 0x03)) as u8;
    d[12] = (width_mm & 0xFF) as u8;
    d[13] = (height_mm & 0xFF) as u8;
    d[14] = ((((width_mm >> 8) & 0x0F) << 4) | ((height_mm >> 8) & 0x0F)) as u8;
    d[15] = 0x00; // horizontal border
    d[16] = 0x00; // vertical border
                  // Flags: digital separate sync, H positive / V negative — the polarity CVT reduced blanking
                  // specifies (and what `drm_cvt_mode` sets for an RB mode).
    d[17] = 0x1A;
    d
}

/// Build the 18-byte display range-limits descriptor (tag 0xFD).
///
/// The window is the **panel envelope widened, if ever necessary, to contain `timing`**. A
/// range-limits block that excludes the very timing DTD-1 declares is self-contradictory — a
/// stricter tell than a merely unusual range, and exactly the state the previous emitter shipped
/// (50–75 Hz / 30–80 kHz / 100 MHz max, against a mode needing 148 kHz and 184 MHz).
fn range_limits(timing: &CvtTiming) -> [u8; 18] {
    let v_rate = timing.refresh_hz().ceil() as u32;
    let h_rate_khz = (timing.line_rate_hz() / 1000.0).ceil() as u32;
    let pclk_mhz = (timing.pixel_clock_hz() as f64 / 1e6).ceil() as u32;

    let v_min = RANGE_MIN_VRATE_HZ.min(v_rate).clamp(1, 255) as u8;
    let h_min = RANGE_MIN_HRATE_KHZ.min(h_rate_khz).clamp(1, 255) as u8;
    let (v_flags, v_max) = max_rate_with_offset(RANGE_MAX_VRATE_HZ.max(v_rate));
    let (h_flags, h_max) = max_rate_with_offset(RANGE_MAX_HRATE_KHZ.max(h_rate_khz));
    // Byte 4: bits 1:0 carry the vertical rate offsets, bits 3:2 the horizontal ones.
    let offset_flags = (h_flags << 2) | v_flags;
    // Byte 9 is the maximum pixel clock in 10 MHz units, rounded up.
    let pclk_max = (RANGE_MAX_PCLK_MHZ.max(pclk_mhz).div_ceil(10)).clamp(1, 255) as u8;

    [
        0x00,
        0x00,
        0x00, // descriptor header (not a detailed timing)
        0xFD, // tag: display range limits
        offset_flags,
        v_min,
        v_max,
        h_min,
        h_max,
        pclk_max,
        0x01, // "range limits only" — no GTF/CVT support block follows
        0x0A,
        0x20,
        0x20,
        0x20,
        0x20,
        0x20,
        0x20, // required padding
    ]
}

/// Encode a maximum rate for the range-limits descriptor as `(offset flag bits, stored byte)`.
///
/// The rate fields are 8-bit, but EDID 1.4 added a per-rate "+255" offset flag (`0b10` in the
/// relevant bit pair of byte 4) precisely so a max above 255 stays expressible — a 120 Hz
/// 1080×2400 panel needs a 305 kHz line rate, so without this the descriptor would silently clamp
/// to 255 and once again exclude the mode DTD-1 declares.
fn max_rate_with_offset(max: u32) -> (u8, u8) {
    if max > 255 {
        (0b10, (max - 255).min(255) as u8)
    } else {
        (0b00, max as u8)
    }
}

/// Whether the base block's 128 bytes sum to 0 mod 256 — the only structural check a guest EDID
/// parser performs before trusting the block.
pub fn checksum_is_valid(edid: &[u8; 128]) -> bool {
    edid.iter().map(|b| *b as u32).sum::<u32>() % 256 == 0
}

/// A detailed timing decoded back **out** of an EDID block — the guest's view of what we claimed.
///
/// Emitting a mode and decoding it are different code paths on purpose: a test that re-derives the
/// expected bytes from the same helper that wrote them proves nothing (the old emitter's test
/// checked only the checksum and non-zero millimetres, which is exactly why a frozen 1024×768 DTD
/// survived). This decoder is also what the VMM's `edid_dtd_agrees_with_get_display_info` guard
/// uses to check the EDID against the scanout it reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecodedTiming {
    /// Horizontal active pixels.
    pub width_px: u32,
    /// Vertical active lines.
    pub height_px: u32,
    /// Total pixels per line.
    pub h_total: u32,
    /// Total lines per frame.
    pub v_total: u32,
    /// Horizontal front porch (sync offset), in pixels.
    pub h_front_porch: u32,
    /// Horizontal sync pulse width, in pixels.
    pub h_sync_width: u32,
    /// Vertical front porch (sync offset), in lines.
    pub v_front_porch: u32,
    /// Vertical sync pulse width, in lines.
    pub v_sync_width: u32,
    /// Pixel clock in Hz.
    pub pixel_clock_hz: u64,
    /// Vertical field rate in Hz, as `pixel_clock / (h_total × v_total)`.
    pub refresh_hz: f32,
    /// Active-area width in millimetres.
    pub width_mm: u32,
    /// Active-area height in millimetres.
    pub height_mm: u32,
}

/// Decode descriptor 1 (bytes 54..72) of an EDID base block as a detailed timing.
///
/// Returns `None` when that descriptor is not a timing at all — a zero pixel clock means the slot
/// holds a display descriptor (monitor name, range limits, …) instead, i.e. the block declares no
/// preferred mode.
pub fn decode_dtd(edid: &[u8; 128]) -> Option<DecodedTiming> {
    let d = &edid[54..72];
    let pclk_10khz = u16::from_le_bytes([d[0], d[1]]) as u64;
    if pclk_10khz == 0 {
        return None;
    }
    let width_px = d[2] as u32 | (((d[4] as u32) >> 4) << 8);
    let h_blank = d[3] as u32 | ((d[4] as u32 & 0x0F) << 8);
    let height_px = d[5] as u32 | (((d[7] as u32) >> 4) << 8);
    let v_blank = d[6] as u32 | ((d[7] as u32 & 0x0F) << 8);
    let h_front_porch = d[8] as u32 | (((d[11] as u32 >> 6) & 0x03) << 8);
    let h_sync_width = d[9] as u32 | (((d[11] as u32 >> 4) & 0x03) << 8);
    let v_front_porch = ((d[10] as u32) >> 4) | (((d[11] as u32 >> 2) & 0x03) << 4);
    let v_sync_width = (d[10] as u32 & 0x0F) | ((d[11] as u32 & 0x03) << 4);
    let width_mm = d[12] as u32 | (((d[14] as u32) >> 4) << 8);
    let height_mm = d[13] as u32 | ((d[14] as u32 & 0x0F) << 8);

    let h_total = width_px + h_blank;
    let v_total = height_px + v_blank;
    let pixel_clock_hz = pclk_10khz * PCLK_UNIT_HZ;
    let refresh_hz = if h_total > 0 && v_total > 0 {
        (pixel_clock_hz as f64 / (h_total as f64 * v_total as f64)) as f32
    } else {
        0.0
    };

    Some(DecodedTiming {
        width_px,
        height_px,
        h_total,
        v_total,
        h_front_porch,
        h_sync_width,
        v_front_porch,
        v_sync_width,
        pixel_clock_hz,
        refresh_hz,
        width_mm,
        height_mm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The VMM's Linux-path default scanout. The emitter must be genuinely parameterised — the
    /// resolution the *old* emitter hardcoded has to come out of the new one by derivation, not by
    /// accident, or nothing is proven about 1080×2400 either.
    #[test]
    fn edid_is_correct_for_the_linux_default_mode() {
        let (w_mm, h_mm) = (mm_from_dpi(1024, 320), mm_from_dpi(768, 320));
        assert!(w_mm > 0 && h_mm > 0, "dpi-derived mm must be non-zero");
        let edid = build_edid(1024, 768, 60, w_mm, h_mm);

        assert_eq!(edid[0..8], [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        assert!(checksum_is_valid(&edid), "byte 127 must zero the block sum");
        assert_eq!(&edid[35..38], &[0x00, 0x00, 0x00], "established timings");
        assert!(edid[21] > 0 && edid[22] > 0, "cm image size must be > 0");

        let t = decode_dtd(&edid).expect("DTD-1 must be a detailed timing");
        assert_eq!((t.width_px, t.height_px), (1024, 768));
        assert_eq!((t.width_mm, t.height_mm), (w_mm, h_mm));
        // CVT-RB: h_total = 1024 + 160; v_blank = floor(460 / ((1e6/60 - 460)/768)) + 1 = 22.
        assert_eq!((t.h_total, t.v_total), (1184, 790));
        assert_eq!(t.pixel_clock_hz, 56_120_000); // 60 × 1184 × 790 = 56.1216 MHz → 10 kHz grid
        assert!(
            (t.refresh_hz - 60.0).abs() <= 1.0,
            "decoded refresh {} Hz",
            t.refresh_hz
        );
        assert_eq!((t.h_front_porch, t.h_sync_width), (48, 32));
        // 1024x768 is 4:3, the one row of the CVT aspect table this device ever hits.
        assert_eq!((t.v_front_porch, t.v_sync_width), (3, 4));
    }

    #[test]
    fn cvt_reduced_blanking_matches_the_worked_derivations() {
        let phone = cvt_reduced_blanking(1080, 2400, 60);
        assert_eq!(phone.h_total(), 1240);
        assert_eq!(phone.h_blank, 160);
        assert_eq!(phone.v_blank, 69);
        assert_eq!(phone.v_total(), 2469);
        assert_eq!(phone.pixel_clock_hz(), 183_690_000);
        assert!((phone.refresh_hz() - 60.0).abs() < 0.01);
        // ~148.1 kHz line rate — the value the old 30–80 kHz range-limits block excluded.
        assert!((phone.line_rate_hz() / 1000.0 - 148.1).abs() < 0.5);

        let linux = cvt_reduced_blanking(1024, 768, 60);
        assert_eq!((linux.h_total(), linux.v_total()), (1184, 790));
        assert_eq!(linux.pixel_clock_hz(), 56_120_000);
    }

    #[test]
    fn range_limits_descriptor_contains_the_emitted_timing() {
        for (w, h, hz) in [
            (1080u32, 2400u32, 60u32),
            (1024, 768, 60),
            (1080, 2400, 120),
        ] {
            let edid = build_edid(w, h, hz, 66, 147);
            let t = cvt_reduced_blanking(w, h, hz);
            let r = &edid[90..108];
            assert_eq!(r[3], 0xFD, "descriptor 3 must be range limits");
            // Byte 4 carries the EDID 1.4 "+255" offsets: bits 1:0 vertical, bits 3:2 horizontal.
            let v_off = if r[4] & 0b11 == 0b10 { 255.0 } else { 0.0 };
            let h_off = if (r[4] >> 2) & 0b11 == 0b10 {
                255.0
            } else {
                0.0
            };
            let (v_min, h_min, pclk_max) = (r[5] as f64, r[7] as f64, r[9] as f64);
            let (v_max, h_max) = (r[6] as f64 + v_off, r[8] as f64 + h_off);
            let refresh = t.refresh_hz();
            let line_khz = t.line_rate_hz() / 1000.0;
            let pclk_mhz = t.pixel_clock_hz() as f64 / 1e6;
            assert!(
                v_min <= refresh && refresh <= v_max,
                "{w}x{h}@{hz}: {refresh} Hz outside {v_min}..{v_max}"
            );
            assert!(
                h_min <= line_khz && line_khz <= h_max,
                "{w}x{h}@{hz}: {line_khz} kHz outside {h_min}..{h_max}"
            );
            assert!(
                pclk_mhz <= pclk_max * 10.0,
                "{w}x{h}@{hz}: {pclk_mhz} MHz over {} MHz",
                pclk_max * 10.0
            );
        }
        // The reference mode's envelope is exactly the declared panel envelope, offsets unused:
        // 48..121 Hz, 30..200 kHz, 250 MHz max pixel clock.
        let edid = build_edid(1080, 2400, 60, 66, 147);
        assert_eq!(edid[94], 0x00, "no rate offsets needed at 60 Hz");
        assert_eq!(&edid[95..100], &[48, 121, 30, 200, 25]);
    }

    /// `INPUT_PROP_DIRECT` — and therefore every touch event — depends on the guest seeing a
    /// non-zero physical size (`crates/aemulator-hvf/src/virtio/input.rs:38,172`). No caller may
    /// produce a block that loses it, not even one passing zeroes.
    #[test]
    fn physical_size_bytes_are_never_zero() {
        for (w_mm, h_mm) in [(66u32, 147u32), (0, 0), (1, 1), (9, 9), (99_999, 99_999)] {
            let edid = build_edid(1080, 2400, 60, w_mm, h_mm);
            assert!(
                edid[21] > 0 && edid[22] > 0,
                "cm bytes 21/22 zero for {w_mm}x{h_mm} mm"
            );
            let t = decode_dtd(&edid).expect("DTD-1 must decode");
            assert!(
                t.width_mm > 0 && t.height_mm > 0,
                "DTD mm bytes 12..15 zero for {w_mm}x{h_mm} mm"
            );
            assert!(checksum_is_valid(&edid));
        }
    }

    #[test]
    fn no_string_in_the_block_names_the_emulator_or_a_blocklisted_tell() {
        let edid = build_edid(1080, 2400, 60, 66, 147);
        let text: String = edid
            .iter()
            .map(|b| {
                if b.is_ascii_graphic() || *b == b' ' {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        let lower = text.to_ascii_lowercase();
        for needle in ["aemulator", "aemu", "emulator"] {
            assert!(!lower.contains(needle), "EDID leaks `{needle}`: {text}");
        }
        // And nothing the detection catalogue would flag in a guest-readable blob.
        for signal in aemulator_detection::blocklist() {
            assert!(
                !lower.contains(&signal.pattern.to_ascii_lowercase()),
                "EDID contains blocklisted `{}`: {text}",
                signal.pattern
            );
        }
    }

    #[test]
    fn a_degenerate_request_still_yields_a_well_formed_block() {
        // Zero width/height/refresh must not panic or divide by zero — the block stays parseable.
        let edid = build_edid(0, 0, 0, 0, 0);
        assert!(checksum_is_valid(&edid));
        let t = decode_dtd(&edid).expect("a timing is still emitted");
        assert!(t.width_px > 0 && t.height_px > 0 && t.pixel_clock_hz > 0);
    }

    #[test]
    fn decode_dtd_rejects_a_block_whose_first_descriptor_is_not_a_timing() {
        let mut edid = build_edid(1080, 2400, 60, 66, 147);
        edid[54] = 0;
        edid[55] = 0; // zero pixel clock = a display descriptor, not a timing
        assert!(decode_dtd(&edid).is_none());
    }

    #[test]
    fn mm_from_dpi_is_zero_only_for_a_zero_dpi() {
        assert_eq!(mm_from_dpi(1024, 320), 81);
        assert_eq!(mm_from_dpi(768, 320), 60);
        assert_eq!(mm_from_dpi(1024, 0), 0);
    }
}
