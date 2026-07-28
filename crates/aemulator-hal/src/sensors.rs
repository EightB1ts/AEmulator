//! Sensors: the host side of the Cuttlefish sensors sub-HAL, on `/dev/hvc18` + `/dev/hvc19`.
//!
//! ## Why this matters more than "the HAL stops crashing"
//!
//! A phone that reports **no sensors at all** is one of the cheapest virtualization tells there is —
//! stronger than any build-prop string, because no amount of identity spoofing explains a Pixel with
//! no accelerometer. A phone whose accelerometer reads exactly `(0, 0, 9.81)` forever is barely
//! better: real hardware is never that still, and "gravity is perfectly axis-aligned and noiseless"
//! is a one-line check. So this module does not merely satisfy the HAL, it produces motion that is
//! *physically coherent*: one internal pose drives every sensor, so accelerometer, gyroscope,
//! magnetometer and orientation all agree about how the device is being held and moved.
//!
//! ## The wire protocol (confirmed against a live guest, not inferred)
//!
//! The guest sub-HAL is `/vendor/lib64/hw/android.hardware.sensors@2.1-impl.cuttlefish.so`
//! (`goldfish::MultihalSensors`), loaded by `android.hardware.sensors-service.multihal` via
//! `/vendor/etc/sensors/hals.conf`. It opens two virtio-console ports in raw termios mode and speaks
//! the goldfish/qemud ASCII sensor protocol, wrapped in Cuttlefish's `RawMessage` envelope.
//!
//! The first bytes ever written on hvc18 by this image, captured by the VMM's port probe:
//!
//! ```text
//! 00 00 00 00 | 0c 00 00 00 | 6c 69 73 74 2d 73 65 6e 73 6f 72 73
//! command=0     len=12        "list-sensors"
//! ```
//!
//! i.e. [`Framing::RawMessage`] — an 8-byte little-endian header, *not* bare newline-delimited text
//! and *not* the 4-byte big-endian prefix the VMM's older secure-channel code assumed.
//!
//! Control port (hvc18), guest → host:
//!
//! | message | meaning | reply |
//! |---|---|---|
//! | `list-sensors` | which sensors does the host have? | a **decimal bitmask**, parsed with `sscanf("%u")` |
//! | `set:<name>:<0\|1>` | enable/disable one sensor | none |
//! | `set-delay:<ms>` | requested sampling period | none |
//! | `time:<ns>` | the guest's `elapsedRealtimeNano()` | none |
//!
//! The guest ANDs the returned mask with its own supported set, so a bit we assert for a sensor the
//! guest does not know about is simply dropped — but a bit in the **wrong position** mislabels a
//! sensor. [`SENSOR_TABLE`]'s order is therefore load-bearing; see its doc comment.
//!
//! Data port (hvc19), host → guest: `"<name>:<f>[:<f>:<f>]"`, parsed with `"%f:%f:%f"` for 3-axis
//! sensors and `"%f:%ld"` for scalars. Nothing is ever read from the guest on this port.

use crate::framing::Framing;
use crate::port::HalPort;

/// How many values a sensor reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorShape {
    /// Three axes, formatted `name:x:y:z`.
    Vec3,
    /// One value, formatted `name:v`.
    Scalar,
}

/// One sensor the host can serve.
#[derive(Clone, Copy, Debug)]
pub struct SensorDef {
    /// The exact token the guest uses in `set:<name>:1` and expects as a data-line prefix. These are
    /// the goldfish protocol's names, not Android's `SENSOR_TYPE_*` names, and must match byte for byte.
    pub name: &'static str,
    pub shape: SensorShape,
}

/// The sensors this host serves, **in bit order** — index `i` is bit `1 << i` of the `list-sensors`
/// mask.
///
/// This ordering is the goldfish `SensorsList` order that `MultihalSensors` was compiled against. A
/// mismatch does not crash anything: the guest just believes a different sensor is enabled than the
/// one we meant, so values land under the wrong name. The `bit_positions_are_stable` test pins the
/// order so it cannot be changed accidentally, and `sensors list` on the control socket prints the
/// live mapping for confirmation against a running guest.
pub const SENSOR_TABLE: &[SensorDef] = &[
    SensorDef {
        name: "acceleration",
        shape: SensorShape::Vec3,
    },
    SensorDef {
        name: "gyroscope",
        shape: SensorShape::Vec3,
    },
    SensorDef {
        name: "magnetic-field",
        shape: SensorShape::Vec3,
    },
    SensorDef {
        name: "orientation",
        shape: SensorShape::Vec3,
    },
    SensorDef {
        name: "temperature",
        shape: SensorShape::Scalar,
    },
    SensorDef {
        name: "proximity",
        shape: SensorShape::Scalar,
    },
    SensorDef {
        name: "light",
        shape: SensorShape::Scalar,
    },
    SensorDef {
        name: "pressure",
        shape: SensorShape::Scalar,
    },
    SensorDef {
        name: "humidity",
        shape: SensorShape::Scalar,
    },
    SensorDef {
        name: "magnetic-field-uncalibrated",
        shape: SensorShape::Vec3,
    },
    SensorDef {
        name: "gyroscope-uncalibrated",
        shape: SensorShape::Vec3,
    },
    SensorDef {
        name: "hinge-angle0",
        shape: SensorShape::Scalar,
    },
];

/// Standard gravity, m/s². The magnitude a real accelerometer reports at rest.
const GRAVITY: f64 = 9.806_65;

/// Earth's magnetic field in the world frame, µT: (north, up-ish, east) mapped to our world axes.
/// Magnitude ≈ 48 µT, a mid-latitude value — a magnetometer reading zero, or one whose magnitude
/// disagrees with the accelerometer's idea of "down", is an obvious tell.
const MAG_WORLD: [f64; 3] = [22.0, -42.0, 0.5];

/// Default sampling period when the guest has not asked for one.
const DEFAULT_DELAY_MS: u32 = 50;
/// Floor on the sampling period. The guest may request 0 (as fast as possible); honouring that
/// literally would spin the pump thread.
pub const MIN_DELAY_MS: u32 = 20;

/// A device pose plus the natural micro-motion of a hand holding it.
///
/// Everything derives from `t` (seconds since start) so the model is deterministic and testable, with
/// no RNG and therefore no hidden state. The frequencies are deliberately incommensurate so the
/// signal never visibly repeats.
#[derive(Clone, Copy, Debug)]
struct Pose {
    /// Seconds since the model started.
    t: f64,
    /// Base tilt: a phone held in a hand, not lying flat. Radians.
    tilt_x: f64,
    tilt_y: f64,
}

impl Pose {
    fn new() -> Self {
        // ~20° back-tilt and a slight roll: a plausible in-hand reading pose.
        Self {
            t: 0.0,
            tilt_x: 0.35,
            tilt_y: -0.06,
        }
    }

    /// Euler angles (roll, pitch, yaw) in radians at the current instant.
    ///
    /// Three superimposed components, each with a physical justification:
    ///  * **postural drift** (~0.05 Hz) — the slow wander of an arm holding a weight;
    ///  * **breathing** (~0.25 Hz) — chest movement couples into the hand;
    ///  * **physiological tremor** (~9 Hz) — the well-documented hand-tremor band (roughly 8-12 Hz),
    ///    small in amplitude but always present in a live grip.
    fn angles(&self) -> (f64, f64, f64) {
        let t = self.t;
        let drift = 0.020 * (2.0 * std::f64::consts::PI * 0.05 * t).sin();
        let breath = 0.008 * (2.0 * std::f64::consts::PI * 0.25 * t).sin();
        let tremor = 0.0016 * (2.0 * std::f64::consts::PI * 9.1 * t).sin();
        let roll = self.tilt_y + drift * 0.6 + tremor;
        let pitch = self.tilt_x + breath + drift * 0.4 - tremor * 0.7;
        let yaw = 0.10 * (2.0 * std::f64::consts::PI * 0.031 * t).sin();
        (roll, pitch, yaw)
    }

    /// Angular velocity in the device frame, rad/s — the **analytic derivative** of [`Pose::angles`].
    ///
    /// This is the part that makes the model coherent rather than decorative. If the gyroscope were an
    /// independent random walk it would disagree with the accelerometer about rotation, and comparing
    /// the two is a standard emulator/bot check. Because this is literally d/dt of the same angles the
    /// accelerometer is derived from, they cannot disagree.
    fn angular_velocity(&self) -> [f64; 3] {
        let t = self.t;
        let tau = 2.0 * std::f64::consts::PI;
        let d_drift = 0.020 * tau * 0.05 * (tau * 0.05 * t).cos();
        let d_breath = 0.008 * tau * 0.25 * (tau * 0.25 * t).cos();
        let d_tremor = 0.0016 * tau * 9.1 * (tau * 9.1 * t).cos();
        let d_roll = d_drift * 0.6 + d_tremor;
        let d_pitch = d_breath + d_drift * 0.4 - d_tremor * 0.7;
        let d_yaw = 0.10 * tau * 0.031 * (tau * 0.031 * t).cos();
        [d_roll, d_pitch, d_yaw]
    }

    /// Rotate a world-frame vector into the device frame using the current orientation.
    ///
    /// Intrinsic Z-Y-X (yaw, then pitch, then roll), the convention Android's orientation sensor uses.
    fn world_to_device(&self, v: [f64; 3]) -> [f64; 3] {
        let (roll, pitch, yaw) = self.angles();
        let (sr, cr) = roll.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        let (sy, cy) = yaw.sin_cos();
        // Transpose of the Z-Y-X rotation matrix (world→device is the inverse of device→world).
        let m = [
            [cy * cp, sy * cp, -sp],
            [cy * sp * sr - sy * cr, sy * sp * sr + cy * cr, cp * sr],
            [cy * sp * cr + sy * sr, sy * sp * cr - cy * sr, cp * cr],
        ];
        [
            m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
            m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
            m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
        ]
    }
}

/// The host's sensor state: which sensors the guest enabled, the requested rate, the motion model, and
/// any values a caller has pinned explicitly.
///
/// One instance is shared by the control port (hvc18, driven by the vCPU thread) and the data pump
/// (hvc19, driven by a VMM thread), so the embedder wraps it in a mutex.
pub struct SensorsState {
    /// Bitmask of sensors the guest has switched on via `set:<name>:1`.
    enabled: u32,
    delay_ms: u32,
    pose: Pose,
    /// Explicit overrides set through the control plane, by table index. `Some` pins a value and
    /// suspends the motion model for that sensor only.
    overrides: [Option<[f64; 3]>; 32],
    /// Whether the motion model runs at all. Turning it off freezes the device perfectly still, which
    /// is useful for deterministic UI tests and is exactly the tell we otherwise avoid — hence opt-in.
    motion: bool,
    /// Last `time:<ns>` the guest reported, for diagnostics.
    guest_time_ns: u64,
    /// Count of control messages handled.
    requests: u64,
}

impl Default for SensorsState {
    fn default() -> Self {
        Self::new()
    }
}

impl SensorsState {
    pub fn new() -> Self {
        Self {
            enabled: 0,
            delay_ms: DEFAULT_DELAY_MS,
            pose: Pose::new(),
            overrides: [None; 32],
            motion: true,
            guest_time_ns: 0,
            requests: 0,
        }
    }

    /// Bitmask advertised in reply to `list-sensors`: every sensor in [`SENSOR_TABLE`].
    pub fn available_mask(&self) -> u32 {
        let mut m = 0u32;
        for i in 0..SENSOR_TABLE.len().min(32) {
            m |= 1 << i;
        }
        m
    }

    /// Bitmask of currently-enabled sensors.
    pub fn enabled_mask(&self) -> u32 {
        self.enabled
    }

    /// The guest's requested sampling period, floored at [`MIN_DELAY_MS`].
    pub fn delay_ms(&self) -> u32 {
        self.delay_ms.max(MIN_DELAY_MS)
    }

    pub fn requests(&self) -> u64 {
        self.requests
    }

    pub fn motion_enabled(&self) -> bool {
        self.motion
    }

    pub fn set_motion(&mut self, on: bool) {
        self.motion = on;
    }

    /// Advance the motion model. Called by the pump thread with the elapsed wall time.
    pub fn advance(&mut self, dt_secs: f64) {
        if self.motion && dt_secs.is_finite() && dt_secs > 0.0 {
            self.pose.t += dt_secs;
        }
    }

    fn index_of(name: &str) -> Option<usize> {
        SENSOR_TABLE.iter().position(|s| s.name == name)
    }

    /// Pin a sensor's value, suspending the motion model for it. Returns false for an unknown name.
    pub fn set_value(&mut self, name: &str, v: [f64; 3]) -> bool {
        match Self::index_of(name) {
            Some(i) if i < 32 => {
                self.overrides[i] = Some(v);
                true
            }
            _ => false,
        }
    }

    /// Drop an override so the sensor resumes following the motion model.
    pub fn clear_value(&mut self, name: &str) -> bool {
        match Self::index_of(name) {
            Some(i) if i < 32 => {
                self.overrides[i] = None;
                true
            }
            _ => false,
        }
    }

    /// The current reading for one sensor, in Android's SI units and device axes.
    ///
    /// Device frame, per the Android sensor coordinate system: +X right, +Y up (towards the top of the
    /// screen), +Z out of the screen towards the user. At rest, gravity therefore reads as a vector
    /// pointing *away* from the ground in device coordinates — which is why the world-frame vector
    /// below is `+GRAVITY` on Y and not `-GRAVITY`.
    pub fn read(&self, idx: usize) -> [f64; 3] {
        if let Some(v) = self.overrides.get(idx).copied().flatten() {
            return v;
        }
        let Some(def) = SENSOR_TABLE.get(idx) else {
            return [0.0; 3];
        };
        let t = self.pose.t;
        match def.name {
            // Accelerometer: gravity rotated into the device frame. A real one also picks up linear
            // acceleration from the hand, which the tremor/breathing terms in the pose supply through
            // the rotation itself.
            "acceleration" => self.pose.world_to_device([0.0, GRAVITY, 0.0]),
            "gyroscope" => {
                let w = self.pose.angular_velocity();
                // A small constant bias: every real MEMS gyro has one, and an exactly-zero-mean gyro
                // is itself anomalous.
                [w[0] + 0.0012, w[1] - 0.0007, w[2] + 0.0004]
            }
            "gyroscope-uncalibrated" => {
                let w = self.pose.angular_velocity();
                // Uncalibrated = the same rate with the bias left in, slightly larger.
                [w[0] + 0.0031, w[1] - 0.0024, w[2] + 0.0018]
            }
            "magnetic-field" => self.pose.world_to_device(MAG_WORLD),
            "magnetic-field-uncalibrated" => {
                let m = self.pose.world_to_device(MAG_WORLD);
                // Hard-iron offset from the device's own magnets/speaker.
                [m[0] + 3.4, m[1] - 1.9, m[2] + 2.6]
            }
            "orientation" => {
                let (roll, pitch, yaw) = self.pose.angles();
                // Android's legacy orientation sensor reports degrees, azimuth first, and azimuth is
                // in [0, 360).
                let az = yaw.to_degrees().rem_euclid(360.0);
                [az, pitch.to_degrees(), roll.to_degrees()]
            }
            // Ambient light, lux: indoor level with a slow drift. Scalars carry their value in [0].
            "light" => [
                220.0 + 18.0 * (2.0 * std::f64::consts::PI * 0.013 * t).sin(),
                0.0,
                0.0,
            ],
            // Proximity, cm: nothing near the earpiece. Reported at the sensor's far value.
            "proximity" => [5.0, 0.0, 0.0],
            // Barometric pressure, hPa: sea-level standard plus weather-scale drift.
            "pressure" => [
                1013.25 + 0.6 * (2.0 * std::f64::consts::PI * 0.004 * t).sin(),
                0.0,
                0.0,
            ],
            // Ambient temperature, °C — device-internal, so a little above room temperature.
            "temperature" => [
                27.5 + 0.4 * (2.0 * std::f64::consts::PI * 0.006 * t).sin(),
                0.0,
                0.0,
            ],
            // Relative humidity, %.
            "humidity" => [
                43.0 + 1.5 * (2.0 * std::f64::consts::PI * 0.005 * t).sin(),
                0.0,
                0.0,
            ],
            // A non-folding phone reports its hinge fully open.
            "hinge-angle0" => [180.0, 0.0, 0.0],
            _ => [0.0; 3],
        }
    }

    /// Format one data-port line for a sensor, in the shape the guest's `sscanf` expects.
    pub fn format_sample(&self, idx: usize) -> Option<String> {
        let def = SENSOR_TABLE.get(idx)?;
        let v = self.read(idx);
        Some(match def.shape {
            SensorShape::Vec3 => format!("{}:{:.6}:{:.6}:{:.6}", def.name, v[0], v[1], v[2]),
            SensorShape::Scalar => format!("{}:{:.6}", def.name, v[0]),
        })
    }

    /// Data-port lines for every currently-enabled sensor. Empty when the guest has enabled none,
    /// which is the normal state until `SensorService` starts a listener — so the pump must not treat
    /// emptiness as an error.
    pub fn enabled_samples(&self) -> Vec<String> {
        (0..SENSOR_TABLE.len().min(32))
            .filter(|i| self.enabled & (1 << i) != 0)
            .filter_map(|i| self.format_sample(i))
            .collect()
    }

    /// Handle one control-port message; return the reply, if any.
    ///
    /// Unknown messages are ignored rather than answered: the goldfish protocol has no error reply, and
    /// inventing one would desynchronise a guest that is waiting for a specific response.
    pub fn handle_control(&mut self, msg: &str) -> Option<String> {
        self.requests += 1;
        let msg = msg.trim();
        if msg == "list-sensors" {
            return Some(self.available_mask().to_string());
        }
        if let Some(rest) = msg.strip_prefix("set-delay:") {
            if let Ok(ms) = rest.trim().parse::<u32>() {
                self.delay_ms = ms;
            }
            return None;
        }
        if let Some(rest) = msg.strip_prefix("time:") {
            if let Ok(ns) = rest.trim().parse::<u64>() {
                self.guest_time_ns = ns;
            }
            return None;
        }
        if let Some(rest) = msg.strip_prefix("set:") {
            // `set:<name>:<0|1>` — the name itself can contain '-', so split from the RIGHT.
            if let Some((name, flag)) = rest.rsplit_once(':') {
                if let Some(i) = Self::index_of(name) {
                    if i < 32 {
                        match flag.trim() {
                            "1" => self.enabled |= 1 << i,
                            "0" => self.enabled &= !(1 << i),
                            _ => {}
                        }
                    }
                }
            }
            return None;
        }
        None
    }

    /// A human-readable dump of the sensor map: name, bit, enabled flag, and current reading. Used by
    /// the control-plane `sensors list` verb, and the practical way to confirm [`SENSOR_TABLE`]'s bit
    /// order against a live guest.
    pub fn describe(&self) -> Vec<String> {
        (0..SENSOR_TABLE.len().min(32))
            .filter_map(|i| {
                let def = SENSOR_TABLE.get(i)?;
                let on = self.enabled & (1 << i) != 0;
                let pinned = self.overrides.get(i).copied().flatten().is_some();
                let v = self.read(i);
                Some(format!(
                    "{}=bit{} {} {}{}",
                    def.name,
                    i,
                    if on { "on" } else { "off" },
                    match def.shape {
                        SensorShape::Vec3 => format!("[{:.3},{:.3},{:.3}]", v[0], v[1], v[2]),
                        SensorShape::Scalar => format!("{:.3}", v[0]),
                    },
                    if pinned { " (pinned)" } else { "" }
                ))
            })
            .collect()
    }
}

/// Framing for the hvc18 **control** port. Confirmed from the live guest's first write:
/// `00 00 00 00 | 0c 00 00 00 | "list-sensors"`.
pub const SENSORS_CONTROL_FRAMING: Framing = Framing::RawMessage;

/// Framing for the hvc19 **data** port.
///
/// Unlike the control port this could not be observed directly: the guest never writes on the data
/// port, it only reads, so there are no guest bytes to hex-dump. It is set to match the control port
/// because both fds are wired into the same `cuttlefish::sensors::SensorsHalProxy` (which takes all
/// four `SharedFD`s and builds `transport::SharedFdChannel`s over them) and the control side is
/// confirmed `RawMessage`. That is strong symmetry but it is an inference, so it is named here as a
/// single flip-point: if `dumpsys sensorservice` shows sensors registered but values never updating,
/// change this to [`Framing::Newline`] and nothing else needs to move.
pub const SENSORS_DATA_FRAMING: Framing = Framing::RawMessage;

/// The hvc18 control port: a [`HalPort`] over a shared [`SensorsState`].
///
/// The state is shared rather than owned because the hvc19 data pump samples the same instance from a
/// different thread — the control port is written by the vCPU thread that takes the MMIO trap, the
/// pump by a VMM timer thread.
pub struct SensorsControlPort {
    state: std::sync::Arc<std::sync::Mutex<SensorsState>>,
}

impl SensorsControlPort {
    pub fn new(state: std::sync::Arc<std::sync::Mutex<SensorsState>>) -> Self {
        Self { state }
    }
}

impl HalPort for SensorsControlPort {
    fn label(&self) -> &'static str {
        "sensors-control(hvc18)"
    }

    fn framing(&self) -> Framing {
        SENSORS_CONTROL_FRAMING
    }

    fn on_message(&mut self, msg: &[u8]) -> Vec<Vec<u8>> {
        // Non-UTF8 on this port would mean we have lost framing; drop rather than guess.
        let Ok(text) = std::str::from_utf8(msg) else {
            return Vec::new();
        };
        // A poisoned lock means another thread panicked while holding the sensor state. Recover the
        // data instead of propagating: this runs on the vCPU thread inside the PCI bus lock, so
        // panicking here would take down the whole virtual bus.
        let mut st = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match st.handle_control(text) {
            Some(r) => vec![r.into_bytes()],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> SensorsState {
        SensorsState::new()
    }

    #[test]
    fn list_sensors_advertises_every_table_entry() {
        let mut s = st();
        let reply = s.handle_control("list-sensors").expect("must reply");
        let mask: u32 = reply
            .parse()
            .expect("reply is a decimal integer for sscanf %u");
        for i in 0..SENSOR_TABLE.len() {
            assert!(
                mask & (1 << i) != 0,
                "bit {i} missing from the advertised mask"
            );
        }
    }

    /// The guest ANDs our mask with its own and then addresses sensors by bit, so a reordering of the
    /// table silently relabels sensors. Pin the order.
    #[test]
    fn bit_positions_are_stable() {
        let expected = [
            "acceleration",
            "gyroscope",
            "magnetic-field",
            "orientation",
            "temperature",
            "proximity",
            "light",
            "pressure",
            "humidity",
            "magnetic-field-uncalibrated",
            "gyroscope-uncalibrated",
            "hinge-angle0",
        ];
        let actual: Vec<&str> = SENSOR_TABLE.iter().map(|d| d.name).collect();
        assert_eq!(actual, expected);
        assert!(SENSOR_TABLE.len() <= 32, "the mask is a u32");
    }

    #[test]
    fn set_enables_and_disables_by_name_including_hyphenated_names() {
        let mut s = st();
        assert_eq!(s.enabled_mask(), 0);
        // The name contains hyphens AND the value is separated by a colon — parsing must split from
        // the right or "magnetic-field-uncalibrated" breaks.
        assert!(s
            .handle_control("set:magnetic-field-uncalibrated:1")
            .is_none());
        let bit = SENSOR_TABLE
            .iter()
            .position(|d| d.name == "magnetic-field-uncalibrated")
            .unwrap();
        assert_eq!(s.enabled_mask(), 1 << bit);
        assert!(s
            .handle_control("set:magnetic-field-uncalibrated:0")
            .is_none());
        assert_eq!(s.enabled_mask(), 0);
    }

    #[test]
    fn unknown_sensor_and_garbage_are_ignored_without_reply() {
        let mut s = st();
        assert!(s.handle_control("set:no-such-sensor:1").is_none());
        assert_eq!(s.enabled_mask(), 0);
        assert!(s.handle_control("").is_none());
        assert!(s.handle_control("::::").is_none());
        assert!(s.handle_control("set:").is_none());
        assert!(s.handle_control("set-delay:notanumber").is_none());
    }

    #[test]
    fn set_delay_is_honoured_but_floored() {
        let mut s = st();
        s.handle_control("set-delay:200");
        assert_eq!(s.delay_ms(), 200);
        // The guest may legitimately ask for 0 ("as fast as you can"); the pump must not spin.
        s.handle_control("set-delay:0");
        assert_eq!(s.delay_ms(), MIN_DELAY_MS);
    }

    #[test]
    fn only_enabled_sensors_are_sampled() {
        let mut s = st();
        assert!(s.enabled_samples().is_empty(), "nothing enabled yet");
        s.handle_control("set:light:1");
        let lines = s.enabled_samples();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("light:"), "got {:?}", lines[0]);
    }

    #[test]
    fn sample_formatting_matches_the_guest_scanf_shapes() {
        let mut s = st();
        s.handle_control("set:acceleration:1");
        s.handle_control("set:pressure:1");
        let lines = s.enabled_samples();
        let accel = lines
            .iter()
            .find(|l| l.starts_with("acceleration:"))
            .unwrap();
        // "%f:%f:%f" after the name => exactly three colon-separated numbers.
        assert_eq!(accel.matches(':').count(), 3, "vec3 line: {accel}");
        let press = lines.iter().find(|l| l.starts_with("pressure:")).unwrap();
        assert_eq!(press.matches(':').count(), 1, "scalar line: {press}");
    }

    /// Gravity must have the right magnitude in every pose, or "is |accel| ≈ 9.81?" flags the device.
    #[test]
    fn accelerometer_magnitude_is_always_gravity() {
        let mut s = st();
        for _ in 0..500 {
            s.advance(0.02);
            let a = s.read(0);
            let mag = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
            assert!(
                (mag - GRAVITY).abs() < 1e-9,
                "|accel| = {mag}, expected {GRAVITY}"
            );
        }
    }

    /// The whole point of the coherent model: the gyroscope must be the derivative of the same
    /// orientation the accelerometer is rotated by. Compare the analytic rate against a numerical
    /// difference of the pose angles.
    #[test]
    fn gyroscope_agrees_with_the_numerical_derivative_of_orientation() {
        let mut s = st();
        s.advance(0.37); // move off t=0 so no term is at a stationary point
        let h = 1e-6;
        let before = s.pose.angles();
        let analytic = s.pose.angular_velocity();
        s.advance(h);
        let after = s.pose.angles();
        let numeric = [
            (after.0 - before.0) / h,
            (after.1 - before.1) / h,
            (after.2 - before.2) / h,
        ];
        for k in 0..3 {
            assert!(
                (analytic[k] - numeric[k]).abs() < 1e-4,
                "axis {k}: analytic {} vs numeric {}",
                analytic[k],
                numeric[k]
            );
        }
    }

    /// A perfectly static accelerometer is itself a tell, so the model must actually move — and move
    /// by a small, physical amount rather than jumping around.
    #[test]
    fn readings_move_over_time_but_stay_physically_small() {
        let mut s = st();
        let first = s.read(0);
        let mut max_delta: f64 = 0.0;
        let mut moved = false;
        for _ in 0..200 {
            s.advance(0.02);
            let a = s.read(0);
            for k in 0..3 {
                let d = (a[k] - first[k]).abs();
                max_delta = max_delta.max(d);
                if d > 1e-4 {
                    moved = true;
                }
            }
        }
        assert!(
            moved,
            "accelerometer never changed — that is the tell we are avoiding"
        );
        assert!(
            max_delta < 1.5,
            "motion of {max_delta} m/s² is not a hand holding a phone"
        );
    }

    #[test]
    fn motion_can_be_frozen_for_deterministic_tests() {
        let mut s = st();
        s.set_motion(false);
        let a = s.read(0);
        for _ in 0..50 {
            s.advance(0.02);
        }
        assert_eq!(s.read(0), a, "motion disabled must freeze the pose exactly");
    }

    #[test]
    fn overrides_pin_a_value_and_can_be_cleared() {
        let mut s = st();
        assert!(s.set_value("acceleration", [1.0, 2.0, 3.0]));
        s.advance(1.0);
        assert_eq!(s.read(0), [1.0, 2.0, 3.0], "pinned value ignores the model");
        assert!(s.clear_value("acceleration"));
        let a = s.read(0);
        assert_ne!(a, [1.0, 2.0, 3.0]);
        assert!(!s.set_value("no-such-sensor", [0.0; 3]));
    }

    #[test]
    fn orientation_azimuth_stays_in_range() {
        let mut s = st();
        let oi = SENSOR_TABLE
            .iter()
            .position(|d| d.name == "orientation")
            .unwrap();
        for _ in 0..1000 {
            s.advance(0.05);
            let o = s.read(oi);
            assert!(
                (0.0..360.0).contains(&o[0]),
                "azimuth {} out of [0,360)",
                o[0]
            );
        }
    }

    #[test]
    fn magnetometer_magnitude_is_constant_and_earth_like() {
        let mut s = st();
        let mi = SENSOR_TABLE
            .iter()
            .position(|d| d.name == "magnetic-field")
            .unwrap();
        let expect = (MAG_WORLD[0].powi(2) + MAG_WORLD[1].powi(2) + MAG_WORLD[2].powi(2)).sqrt();
        assert!(
            (25.0..70.0).contains(&expect),
            "field magnitude {expect} µT is not earth-like"
        );
        for _ in 0..200 {
            s.advance(0.02);
            let m = s.read(mi);
            let mag = (m[0] * m[0] + m[1] * m[1] + m[2] * m[2]).sqrt();
            assert!((mag - expect).abs() < 1e-9, "|mag| drifted to {mag}");
        }
    }

    #[test]
    fn describe_lists_every_sensor_with_its_bit() {
        let s = st();
        let lines = s.describe();
        assert_eq!(lines.len(), SENSOR_TABLE.len());
        assert!(
            lines[0].starts_with("acceleration=bit0"),
            "got {:?}",
            lines[0]
        );
    }

    #[test]
    fn control_port_replies_only_to_list_sensors() {
        use std::sync::{Arc, Mutex};
        let state = Arc::new(Mutex::new(SensorsState::new()));
        let mut port = SensorsControlPort::new(state.clone());
        let replies = port.on_message(b"list-sensors");
        assert_eq!(replies.len(), 1);
        assert!(!replies[0].is_empty());
        assert!(port.on_message(b"set-delay:100").is_empty());
        assert!(port.on_message(b"time:12345").is_empty());
        // Invalid UTF-8 means lost framing: drop, do not panic.
        assert!(port.on_message(&[0xff, 0xfe, 0x00]).is_empty());
        assert_eq!(port.framing(), SENSORS_CONTROL_FRAMING);
        // Commands reached the shared state, which the data pump also reads.
        assert_eq!(state.lock().unwrap().delay_ms(), 100);
    }

    /// A panic on the vCPU thread inside the PCI bus lock would kill the VM, so the control port must
    /// keep working even after another thread poisoned the sensor mutex.
    #[test]
    fn control_port_survives_a_poisoned_lock() {
        use std::sync::{Arc, Mutex};
        let state = Arc::new(Mutex::new(SensorsState::new()));
        let s2 = state.clone();
        let _ = std::thread::spawn(move || {
            let _g = s2.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert!(state.is_poisoned());
        let mut port = SensorsControlPort::new(state);
        assert_eq!(port.on_message(b"list-sensors").len(), 1);
    }

    #[test]
    fn fuzz_lite_never_panics() {
        let mut x: u32 = 0xdead_beef;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut s = st();
        let alphabet = b"set:-0123456789listnors.dely:tim";
        for _ in 0..50_000 {
            let n = (next() % 24) as usize;
            let msg: String = (0..n)
                .map(|_| alphabet[(next() as usize) % alphabet.len()] as char)
                .collect();
            let _ = s.handle_control(&msg);
            let _ = s.enabled_samples();
            s.advance(0.001);
        }
        // Extreme times must not produce NaN/inf readings.
        s.pose.t = 1.0e9;
        for i in 0..SENSOR_TABLE.len() {
            let v = s.read(i);
            for (k, axis) in v.iter().enumerate() {
                assert!(axis.is_finite(), "sensor {i} axis {k} is not finite");
            }
        }
    }
}
