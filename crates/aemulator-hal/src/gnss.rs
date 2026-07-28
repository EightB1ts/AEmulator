//! GNSS: NMEA 0183 sentence synthesis for the guest's GNSS HAL on `/dev/hvc6` and `/dev/hvc7`.
//!
//! ## What the guest does
//!
//! `init.cutf_cvm.rc` symlinks `/dev/hvc6` → `/dev/gnss0` and `/dev/hvc7` → `/dev/gnss1`, and the AOSP
//! reference HAL (`android.hardware.gnss-service.example`, path overridable via `vendor.ser.gnss-uart`)
//! reads location data from them. It is **read-only from our side**: the boot probe recorded no guest
//! writes on either port at all, which is why this module has no [`crate::HalPort`] — there is nothing
//! to respond to, only a stream to produce.
//!
//! ## Format, and what is inference rather than observation
//!
//! On real Cuttlefish the far end is `gnss_grpc_proxy`, whose gRPC surface is `SendGps`,
//! `SendGpsVector` and `SendNmea`; its `SendNmeaRequest.nmea` field is a plain string and its log line
//! is `"Send Gnss Raw to serial: bytes_written: "`. So the proxy forwards **unframed** bytes to the
//! guest, and for the `SendNmea` path those bytes are NMEA text supplied by its caller. Two things
//! follow, and the second is honestly a deduction:
//!
//! * the wire carries raw bytes with no Cuttlefish envelope — supported by that log string; and
//! * the content is NMEA sentences — supported by `SendNmea` existing and by the reference HAL being a
//!   serial-NMEA reader, but **not** directly observed, because `gnss_grpc_proxy` contains no
//!   `$GPGGA`-style literals of its own (it relays what its caller sends).
//!
//! Which of hvc6/hvc7 is which also could not be determined: the proxy has separate `gnss_in/out_fd`
//! and `fixed_location_in/out_fd` pairs, and nothing ties either to a specific console index. Emitting
//! the same stream on both is therefore deliberate — it costs one extra write per second and removes
//! the guess.

/// A geographic fix to broadcast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    pub latitude: f64,
    pub longitude: f64,
    /// Metres above mean sea level.
    pub altitude: f64,
    /// Ground speed, knots.
    pub speed_knots: f64,
    /// True course, degrees.
    pub course_deg: f64,
    /// Satellites used in the fix.
    pub satellites: u8,
    /// Horizontal dilution of precision.
    pub hdop: f64,
}

impl Default for Fix {
    fn default() -> Self {
        // Googleplex, Mountain View — the same default location the Android emulator ships, so a device
        // that has never been told where it is still reports somewhere coherent rather than (0, 0), the
        // "null island" reading that is itself a tell.
        Self {
            latitude: 37.421_998,
            longitude: -122.084_000,
            altitude: 9.0,
            speed_knots: 0.0,
            course_deg: 0.0,
            satellites: 12,
            hdop: 0.8,
        }
    }
}

/// Produces NMEA sentences for a fix.
///
/// Time is supplied by the caller rather than read from the clock, so output is deterministic and
/// testable — and so the VMM can drive it from the same tick as the rest of its pumps.
pub struct NmeaSource {
    fix: Fix,
}

impl Default for NmeaSource {
    fn default() -> Self {
        Self::new()
    }
}

impl NmeaSource {
    pub fn new() -> Self {
        Self {
            fix: Fix::default(),
        }
    }

    pub fn fix(&self) -> Fix {
        self.fix
    }

    pub fn set_fix(&mut self, fix: Fix) {
        self.fix = fix;
    }

    /// Set just the position, keeping the rest of the fix. This is what the control plane's `gps` verb
    /// needs.
    pub fn set_position(&mut self, lat: f64, lon: f64) {
        self.fix.latitude = lat;
        self.fix.longitude = lon;
    }

    /// NMEA checksum: XOR of every byte between `$` and `*`, as two uppercase hex digits.
    fn checksum(body: &str) -> String {
        let sum = body.bytes().fold(0u8, |a, b| a ^ b);
        format!("{sum:02X}")
    }

    /// Wrap a sentence body as `$<body>*<checksum>\r\n`.
    fn sentence(body: &str) -> String {
        format!("${}*{}\r\n", body, Self::checksum(body))
    }

    /// Format a coordinate in NMEA's `ddmm.mmmm` / `dddmm.mmmm` form plus its hemisphere.
    ///
    /// The degrees field is fixed-width and the minutes are decimal — not decimal degrees, which is the
    /// classic way to produce sentences that parse but land in the wrong place.
    fn coord(value: f64, is_latitude: bool) -> (String, char) {
        let hemi = if is_latitude {
            if value >= 0.0 {
                'N'
            } else {
                'S'
            }
        } else if value >= 0.0 {
            'E'
        } else {
            'W'
        };
        let abs = value.abs();
        let deg = abs.trunc() as u32;
        let minutes = (abs - deg as f64) * 60.0;
        let s = if is_latitude {
            format!("{deg:02}{minutes:07.4}")
        } else {
            format!("{deg:03}{minutes:07.4}")
        };
        (s, hemi)
    }

    /// `$GPGGA` — fix data: time, position, quality, satellites, HDOP, altitude.
    pub fn gga(&self, utc_seconds_of_day: u32) -> String {
        let (h, m, s) = hms(utc_seconds_of_day);
        let (lat, ns) = Self::coord(self.fix.latitude, true);
        let (lon, ew) = Self::coord(self.fix.longitude, false);
        let body = format!(
            "GPGGA,{h:02}{m:02}{s:02}.00,{lat},{ns},{lon},{ew},1,{:02},{:.1},{:.1},M,0.0,M,,",
            self.fix.satellites, self.fix.hdop, self.fix.altitude
        );
        Self::sentence(&body)
    }

    /// `$GPRMC` — recommended minimum: time, validity, position, speed, course, date.
    pub fn rmc(&self, utc_seconds_of_day: u32, date_ddmmyy: &str) -> String {
        let (h, m, s) = hms(utc_seconds_of_day);
        let (lat, ns) = Self::coord(self.fix.latitude, true);
        let (lon, ew) = Self::coord(self.fix.longitude, false);
        let body = format!(
            "GPRMC,{h:02}{m:02}{s:02}.00,A,{lat},{ns},{lon},{ew},{:.2},{:.2},{date_ddmmyy},,,A",
            self.fix.speed_knots, self.fix.course_deg
        );
        Self::sentence(&body)
    }

    /// `$GPGSA` — DOP and active satellites. Reporting a 3D fix keeps the HAL from treating the fix as
    /// horizontal-only, which would suppress altitude.
    pub fn gsa(&self) -> String {
        let body = format!(
            "GPGSA,A,3,01,02,03,04,05,06,07,08,09,10,11,12,{:.1},{:.1},{:.1}",
            self.fix.hdop * 1.5,
            self.fix.hdop,
            self.fix.hdop * 1.2
        );
        Self::sentence(&body)
    }

    /// One second's worth of sentences, in the order a real receiver emits them.
    pub fn burst(&self, utc_seconds_of_day: u32, date_ddmmyy: &str) -> String {
        let mut s = String::new();
        s.push_str(&self.gga(utc_seconds_of_day));
        s.push_str(&self.rmc(utc_seconds_of_day, date_ddmmyy));
        s.push_str(&self.gsa());
        s
    }
}

/// Split a seconds-of-day count into hours, minutes and seconds.
fn hms(secs: u32) -> (u32, u32, u32) {
    let s = secs % 86_400;
    (s / 3600, (s % 3600) / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify a sentence the way a receiver's parser would: `$`, body, `*`, correct checksum, CRLF.
    fn assert_valid_nmea(sentence: &str) {
        assert!(sentence.starts_with('$'), "must start with $: {sentence:?}");
        assert!(sentence.ends_with("\r\n"), "must end CRLF: {sentence:?}");
        let core = &sentence[1..sentence.len() - 2];
        let (body, sum) = core
            .split_once('*')
            .expect("must contain a * checksum delimiter");
        assert_eq!(sum.len(), 2, "checksum is two hex digits: {sum:?}");
        let expect = body.bytes().fold(0u8, |a, b| a ^ b);
        assert_eq!(
            u8::from_str_radix(sum, 16).expect("checksum must be hex"),
            expect,
            "checksum mismatch in {sentence:?}"
        );
        assert!(
            sentence.len() <= 82,
            "NMEA 0183 caps a sentence at 82 bytes, got {}",
            sentence.len()
        );
    }

    #[test]
    fn gga_and_rmc_are_valid_nmea() {
        let src = NmeaSource::new();
        assert_valid_nmea(&src.gga(12 * 3600 + 34 * 60 + 56));
        assert_valid_nmea(&src.rmc(12 * 3600 + 34 * 60 + 56, "280726"));
        assert_valid_nmea(&src.gsa());
    }

    #[test]
    fn every_sentence_in_a_burst_is_valid() {
        let src = NmeaSource::new();
        let burst = src.burst(3600, "010125");
        let lines: Vec<&str> = burst.split_inclusive("\r\n").collect();
        assert_eq!(lines.len(), 3, "GGA + RMC + GSA");
        for l in lines {
            assert_valid_nmea(l);
        }
    }

    /// Coordinates must be `ddmm.mmmm`, not decimal degrees — the sentence would still parse but the
    /// position would be wrong, which is far worse than a parse failure.
    #[test]
    fn coordinates_use_degrees_and_decimal_minutes() {
        let (s, hemi) = NmeaSource::coord(37.421_998, true);
        assert_eq!(hemi, 'N');
        // 37.421998° = 37° + 0.421998*60 = 25.31988'
        assert_eq!(s, "3725.3199", "got {s}");
        let (s, hemi) = NmeaSource::coord(-122.084_000, false);
        assert_eq!(hemi, 'W', "negative longitude is West");
        // 122.084° = 122° + 5.04'
        assert_eq!(s, "12205.0400", "got {s}");
        // Longitude is zero-padded to three degree digits, latitude to two.
        let (s, _) = NmeaSource::coord(5.5, false);
        assert!(s.starts_with("005"), "longitude degrees are 3 digits: {s}");
        let (s, _) = NmeaSource::coord(5.5, true);
        assert!(s.starts_with("05"), "latitude degrees are 2 digits: {s}");
    }

    #[test]
    fn southern_and_eastern_hemispheres_are_signed_correctly() {
        assert_eq!(NmeaSource::coord(-33.86, true).1, 'S');
        assert_eq!(NmeaSource::coord(151.20, false).1, 'E');
    }

    #[test]
    fn time_is_formatted_as_hhmmss() {
        let src = NmeaSource::new();
        let g = src.gga(3600 + 2 * 60 + 3); // 01:02:03
        assert!(g.contains("GPGGA,010203.00,"), "got {g}");
        // Seconds-of-day beyond a day must wrap rather than produce an invalid hour.
        let g = src.gga(86_400 + 61);
        assert!(g.contains("GPGGA,000101.00,"), "got {g}");
    }

    /// The default must not be (0, 0): a device reporting null island is as much a tell as one with no
    /// GPS at all.
    #[test]
    fn default_fix_is_a_real_place_with_a_usable_satellite_count() {
        let f = Fix::default();
        assert!(
            f.latitude.abs() > 1.0 && f.longitude.abs() > 1.0,
            "not null island"
        );
        assert!((-90.0..=90.0).contains(&f.latitude));
        assert!((-180.0..=180.0).contains(&f.longitude));
        assert!(f.satellites >= 4, "a 3D fix needs at least 4 satellites");
        assert!(
            f.hdop > 0.0 && f.hdop < 5.0,
            "HDOP should indicate a good fix"
        );
    }

    #[test]
    fn set_position_updates_the_sentences_and_keeps_the_rest_of_the_fix() {
        let mut src = NmeaSource::new();
        let sats = src.fix().satellites;
        src.set_position(51.5074, -0.1278); // London
        let g = src.gga(0);
        assert!(g.contains("5130.4440,N"), "got {g}");
        assert!(g.contains("00007.6680,W"), "got {g}");
        assert_valid_nmea(&g);
        assert_eq!(src.fix().satellites, sats, "satellite count preserved");
    }

    #[test]
    fn extreme_coordinates_still_produce_valid_sentences() {
        let mut src = NmeaSource::new();
        for (lat, lon) in [
            (0.0, 0.0),
            (89.999_999, 179.999_999),
            (-89.999_999, -179.999_999),
        ] {
            src.set_position(lat, lon);
            assert_valid_nmea(&src.gga(43_200));
            assert_valid_nmea(&src.rmc(43_200, "280726"));
        }
    }
}
