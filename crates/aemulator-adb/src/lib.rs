//! # aemulator-adb — real device backends
//!
//! The default [`aemulator_supervisor::MockLauncher`] + [`aemulator_control::MockController`] prove
//! the *seams*; this crate provides the **real** ones so `aemulator` actually boots and drives a
//! live ARM64 Android VM on this host:
//!
//! - [`AdbController`] — a real [`DeviceController`](aemulator_control::DeviceController) over `adb`:
//!   `observe()` captures a live framebuffer PNG + the structured UI tree (`uiautomator`), and
//!   `act()` injects real taps/swipes/text/keys/app-launches at the device.
//! - [`AndroidHvfLauncher`] — a real [`Launcher`](aemulator_supervisor::Launcher) that boots an
//!   ARM64 Android guest **in a window under Hypervisor.framework** (Apple Silicon), injects the
//!   writable subset of a [`DeviceProfile`]'s identity, waits for boot, and returns a live
//!   [`AdbController`].
//!
//! ## Honest scope (macOS dev loop vs the stealth fleet)
//!
//! On macOS we ride the platform's Android/HVF engine (the goldfish/ranchu device model) as the
//! **execution substrate** — this is the plan's "QEMU-as-bring-up-oracle / HVF dev loop". Our
//! value-add here is real **orchestration + identity injection + control + live audit**. The
//! ranchu device model still bakes deep tells into the *image* (`ro.hardware=ranchu`,
//! `/vendor/etc/fstab.ranchu`, the Goldfish sensor HAL) that a runtime `-prop` cannot rewrite —
//! clearing those to zero is the **custom de-branded guest image + crosvm/KVM fleet** milestone
//! (`aemulator-vmm::crosvm_backend`). `aemulator audit` reports exactly which tells remain, so the
//! boundary is measured, never hand-waved.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use aemulator_control::{
    Action, ControlError, DeviceController, Observation, Rect, UiNode, UiTree,
};
use aemulator_identity::DeviceProfile;
use aemulator_supervisor::{Launched, Launcher, SupervisorError};
use aemulator_vmm::BootAssets;

/// Locate the Android SDK root (`ANDROID_SDK_ROOT`/`ANDROID_HOME`, else the macOS default).
pub fn sdk_root() -> PathBuf {
    for var in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Android/sdk")
}

/// Absolute path to `adb` (SDK `platform-tools/adb`, else bare `adb` on `PATH`).
pub fn adb_path() -> PathBuf {
    let p = sdk_root().join("platform-tools/adb");
    if p.exists() {
        p
    } else {
        PathBuf::from("adb")
    }
}

/// Absolute path to the SDK `emulator` binary (else bare `emulator` on `PATH`).
pub fn emulator_path() -> PathBuf {
    let p = sdk_root().join("emulator/emulator");
    if p.exists() {
        p
    } else {
        PathBuf::from("emulator")
    }
}

fn be(msg: impl Into<String>) -> ControlError {
    ControlError::Backend(msg.into())
}

/// A live [`DeviceController`] backed by `adb`. Perception is a real screencap + `uiautomator`
/// dump; actions are real `input`/`am`/`adb install` invocations.
pub struct AdbController {
    adb: PathBuf,
    serial: String,
    /// Mutations performed. Advances on every mutating `act`, never rewound (see the control
    /// contract's generation invariant). `observe` tags the tree with the current value.
    generation: u64,
    /// The tree captured by the most recent `observe`, used to resolve `Action::Tap { index }`.
    last_tree: RefCell<UiTree>,
}

impl AdbController {
    /// Connect to a running device. `serial = None` auto-selects the first `emulator-*` (or any)
    /// device reported by `adb devices`.
    pub fn connect(serial: Option<String>) -> Result<Self, ControlError> {
        let adb = adb_path();
        let _ = Command::new(&adb).arg("start-server").status();
        let serial = match serial {
            Some(s) => s,
            None => Self::pick_device(&adb)?,
        };
        Ok(Self {
            adb,
            serial,
            generation: 0,
            last_tree: RefCell::new(UiTree::default()),
        })
    }

    /// The adb serial this controller is bound to (e.g. `emulator-5554`).
    pub fn serial(&self) -> &str {
        &self.serial
    }

    fn pick_device(adb: &Path) -> Result<String, ControlError> {
        let out = Command::new(adb)
            .arg("devices")
            .output()
            .map_err(|e| be(format!("adb devices: {e}")))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut emu = None;
        let mut any = None;
        for line in text.lines().skip(1) {
            let mut it = line.split_whitespace();
            if let (Some(serial), Some(state)) = (it.next(), it.next()) {
                if state == "device" {
                    any.get_or_insert_with(|| serial.to_string());
                    if serial.starts_with("emulator-") {
                        emu.get_or_insert_with(|| serial.to_string());
                    }
                }
            }
        }
        emu.or(any)
            .ok_or_else(|| be("no adb device in state `device` (is a VM running?)"))
    }

    /// Run `adb -s <serial> shell <args...>` and return stdout as a string.
    pub fn shell(&self, args: &[&str]) -> Result<String, ControlError> {
        let mut cmd = Command::new(&self.adb);
        cmd.arg("-s").arg(&self.serial).arg("shell").args(args);
        let out = cmd.output().map_err(|e| be(format!("adb shell: {e}")))?;
        if !out.status.success() {
            return Err(be(format!(
                "adb shell {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run `adb -s <serial> exec-out <args...>` and return raw stdout bytes (for binary payloads).
    fn exec_out(&self, args: &[&str]) -> Result<Vec<u8>, ControlError> {
        let out = Command::new(&self.adb)
            .arg("-s")
            .arg(&self.serial)
            .arg("exec-out")
            .args(args)
            .output()
            .map_err(|e| be(format!("adb exec-out: {e}")))?;
        if !out.status.success() {
            return Err(be(format!(
                "adb exec-out {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(out.stdout)
    }

    /// Whether a path exists on the device (`test -e`).
    pub fn exists(&self, path: &str) -> bool {
        self.shell(&["sh", "-c", &format!("test -e {path} && echo Y || echo N")])
            .map(|s| s.trim() == "Y")
            .unwrap_or(false)
    }

    /// Parse `getprop` into a full property map.
    pub fn getprop_all(&self) -> Result<BTreeMap<String, String>, ControlError> {
        let raw = self.shell(&["getprop"])?;
        Ok(parse_getprop(&raw))
    }

    fn screencap_png(&self) -> Result<Vec<u8>, ControlError> {
        self.exec_out(&["screencap", "-p"])
    }

    fn dump_ui(&self) -> Result<Vec<UiNode>, ControlError> {
        // uiautomator prints a status line, not the XML, so dump to a file then read it back.
        let _ = self.shell(&["uiautomator", "dump", "/sdcard/aemu_ui.xml"]);
        let bytes = self
            .exec_out(&["cat", "/sdcard/aemu_ui.xml"])
            .unwrap_or_default();
        Ok(parse_ui_xml(&String::from_utf8_lossy(&bytes)))
    }

    fn input_tap(&self, x: i32, y: i32) -> Result<(), ControlError> {
        self.shell(&["input", "tap", &x.to_string(), &y.to_string()])
            .map(|_| ())
    }

    /// Wait until `sys.boot_completed == 1` or `timeout` elapses.
    pub fn wait_for_boot(&self, timeout: Duration) -> Result<(), ControlError> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self
                .shell(&["getprop", "sys.boot_completed"])
                .map(|s| s.trim() == "1")
                .unwrap_or(false)
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        Err(be("timed out waiting for sys.boot_completed"))
    }
}

impl DeviceController for AdbController {
    fn observe(&self) -> Result<Observation, ControlError> {
        let png = self.screencap_png().unwrap_or_default();
        let (width, height) = png_dimensions(&png).unwrap_or((0, 0));
        let nodes = self.dump_ui().unwrap_or_default();
        let tree = UiTree {
            nodes,
            generation: self.generation,
        };
        *self.last_tree.borrow_mut() = tree.clone();
        Ok(Observation {
            screenshot_png: png,
            width,
            height,
            ui_tree: tree,
            generation: self.generation,
        })
    }

    fn act(&mut self, action: Action) -> Result<Observation, ControlError> {
        match action {
            Action::Tap { index, generation } => {
                if generation != self.generation {
                    return Err(ControlError::StaleGeneration {
                        expected: self.generation,
                        got: generation,
                    });
                }
                let (x, y) = {
                    let tree = self.last_tree.borrow();
                    let node = tree.nodes.get(index as usize).ok_or(
                        ControlError::NodeIndexOutOfRange {
                            index,
                            len: tree.nodes.len(),
                        },
                    )?;
                    node.bounds.center()
                };
                self.input_tap(x, y)?;
                self.generation += 1;
            }
            Action::TapXy { x, y } => {
                self.input_tap(x, y)?;
                self.generation += 1;
            }
            Action::Swipe {
                x1,
                y1,
                x2,
                y2,
                duration_ms,
            } => {
                self.shell(&[
                    "input",
                    "swipe",
                    &x1.to_string(),
                    &y1.to_string(),
                    &x2.to_string(),
                    &y2.to_string(),
                    &duration_ms.to_string(),
                ])?;
                self.generation += 1;
            }
            Action::Text { text } => {
                // `input text` uses %s for spaces.
                let escaped = text.replace(' ', "%s");
                self.shell(&["input", "text", &escaped])?;
                self.generation += 1;
            }
            Action::Key { code } => {
                self.shell(&["input", "keyevent", &code.to_string()])?;
                self.generation += 1;
            }
            Action::LaunchApp { pkg } => {
                self.shell(&[
                    "monkey",
                    "-p",
                    &pkg,
                    "-c",
                    "android.intent.category.LAUNCHER",
                    "1",
                ])?;
                self.generation += 1;
            }
            Action::InstallApk { path } => {
                let out = Command::new(&self.adb)
                    .arg("-s")
                    .arg(&self.serial)
                    .arg("install")
                    .arg("-r")
                    .arg(&path)
                    .output()
                    .map_err(|e| be(format!("adb install: {e}")))?;
                if !out.status.success() {
                    return Err(be(format!(
                        "adb install failed: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )));
                }
                self.generation += 1;
            }
            Action::SetGps { lat, lng } => {
                // `adb emu geo fix <lon> <lat>`
                Command::new(&self.adb)
                    .arg("-s")
                    .arg(&self.serial)
                    .arg("emu")
                    .arg("geo")
                    .arg("fix")
                    .arg(lng.to_string())
                    .arg(lat.to_string())
                    .status()
                    .map_err(|e| be(format!("adb emu geo fix: {e}")))?;
                self.generation += 1;
            }
            Action::WaitStable => {
                self.wait_stable(Duration::from_secs(10))?;
            }
            Action::Snapshot { name } => {
                Command::new(&self.adb)
                    .arg("-s")
                    .arg(&self.serial)
                    .arg("emu")
                    .arg("avd")
                    .arg("snapshot")
                    .arg("save")
                    .arg(&name)
                    .status()
                    .map_err(|e| be(format!("snapshot save: {e}")))?;
            }
            Action::Restore { name } => {
                let st = Command::new(&self.adb)
                    .arg("-s")
                    .arg(&self.serial)
                    .arg("emu")
                    .arg("avd")
                    .arg("snapshot")
                    .arg("load")
                    .arg(&name)
                    .status()
                    .map_err(|e| be(format!("snapshot load: {e}")))?;
                if !st.success() {
                    return Err(ControlError::SnapshotNotFound(name));
                }
            }
        }
        self.observe()
    }
}

impl AdbController {
    /// Poll the framebuffer hash until it is unchanged across two reads or `timeout` elapses.
    fn wait_stable(&self, timeout: Duration) -> Result<(), ControlError> {
        let start = Instant::now();
        let mut last = self.screencap_png().unwrap_or_default();
        while start.elapsed() < timeout {
            std::thread::sleep(Duration::from_millis(600));
            let now = self.screencap_png().unwrap_or_default();
            if !now.is_empty() && now == last {
                return Ok(());
            }
            last = now;
        }
        Ok(())
    }
}

/// A real [`Launcher`] that boots an ARM64 Android guest **in a window under HVF** and returns a live
/// [`AdbController`]. See the crate-level "honest scope" note.
pub struct AndroidHvfLauncher {
    /// The AVD to boot (must already exist; created via `sdkmanager`/`avdmanager`).
    pub avd: String,
    /// GPU mode; `"host"` uses Metal on Apple Silicon for a hardware-accelerated window.
    pub gpu: String,
    /// Show the emulator window (`false` boots headless).
    pub window: bool,
    /// Inject the writable subset of the profile's identity via `-prop` at boot.
    pub inject_identity: bool,
    /// Boot with `-writable-system` so `adb remount` + `harden` can rewrite the image's
    /// `build.prop` identity in place (the legitimate build-time-identity mechanism, applied live).
    pub writable_system: bool,
    /// Max time to wait for `sys.boot_completed`.
    pub boot_timeout: Duration,
}

impl Default for AndroidHvfLauncher {
    fn default() -> Self {
        Self {
            avd: "aemu_ranchu_arm64".to_string(),
            gpu: "host".to_string(),
            window: true,
            inject_identity: true,
            writable_system: false,
            boot_timeout: Duration::from_secs(240),
        }
    }
}

/// The writable identity slots we attempt to inject at boot via the emulator `-prop` channel. Deep
/// image-baked tells (`ro.hardware`, `fstab.ranchu`, the Goldfish sensor HAL) are deliberately NOT
/// here — they cannot be rewritten at runtime and require the custom guest image.
const INJECTABLE_PROP_KEYS: &[&str] = &[
    "ro.product.brand",
    "ro.product.model",
    "ro.product.name",
    "ro.product.device",
    "ro.product.manufacturer",
    "ro.product.board",
    "ro.build.fingerprint",
    "ro.build.tags",
    "ro.build.flavor",
    "ro.build.characteristics",
    "ro.product.brand_for_attestation",
    "ro.product.model_for_attestation",
    "ro.product.name_for_attestation",
    "ro.product.device_for_attestation",
    "ro.product.manufacturer_for_attestation",
];

impl AndroidHvfLauncher {
    /// The curated `(key,value)` identity pairs this launcher will inject for `profile`.
    pub fn injectable_props(&self, profile: &DeviceProfile) -> Vec<(String, String)> {
        if !self.inject_identity {
            return Vec::new();
        }
        let all = profile.emit_properties();
        INJECTABLE_PROP_KEYS
            .iter()
            .filter_map(|k| all.get(*k).map(|v| (k.to_string(), v.clone())))
            .collect()
    }
}

impl Launcher for AndroidHvfLauncher {
    fn launch(
        &self,
        profile: &DeviceProfile,
        _assets: &BootAssets,
    ) -> Result<Launched, SupervisorError> {
        let emu = emulator_path();
        let props = self.injectable_props(profile);

        // Assemble the argv (also returned for the record).
        let mut argv: Vec<String> = vec![
            emu.to_string_lossy().into_owned(),
            "-avd".into(),
            self.avd.clone(),
            "-gpu".into(),
            self.gpu.clone(),
            "-no-snapshot".into(),
            "-netdelay".into(),
            "none".into(),
            "-netspeed".into(),
            "full".into(),
        ];
        if !self.window {
            argv.push("-no-window".into());
        }
        if self.writable_system {
            argv.push("-writable-system".into());
        }
        for (k, v) in &props {
            argv.push("-prop".into());
            argv.push(format!("{k}={v}"));
        }

        // Detach fully via `nohup ... &` so the window survives the CLI process exiting.
        let log = std::env::temp_dir().join(format!("aemulator-{}.log", self.avd));
        let quoted: String = argv
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        let sh = format!("nohup {quoted} > '{}' 2>&1 &", log.display());
        Command::new("/bin/sh")
            .arg("-c")
            .arg(&sh)
            .status()
            .map_err(|e| SupervisorError::Launch(format!("spawning emulator: {e}")))?;

        // Wait for the device to appear, then for boot.
        let serial = wait_for_serial(Duration::from_secs(60)).map_err(SupervisorError::Launch)?;
        let controller = AdbController::connect(Some(serial))
            .map_err(|e| SupervisorError::Launch(format!("connect: {e}")))?;
        controller
            .wait_for_boot(self.boot_timeout)
            .map_err(|e| SupervisorError::Launch(format!("boot wait: {e}")))?;

        Ok(Launched {
            launch_args: argv,
            controller: Box::new(controller),
        })
    }
}

/// Poll `adb devices` until an `emulator-*` device is present, returning its serial.
fn wait_for_serial(timeout: Duration) -> Result<String, String> {
    let adb = adb_path();
    let _ = Command::new(&adb).arg("start-server").status();
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(out) = Command::new(&adb).arg("devices").output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines().skip(1) {
                let mut it = line.split_whitespace();
                if let (Some(serial), Some(state)) = (it.next(), it.next()) {
                    if serial.starts_with("emulator-") && (state == "device" || state == "offline")
                    {
                        // Wait for it to leave `offline`.
                        if state == "device" {
                            return Ok(serial.to_string());
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err("no emulator device appeared".into())
}

/// Parse `adb shell getprop` output (`[key]: [value]` per line) into a map.
pub fn parse_getprop(raw: &str) -> BTreeMap<String, String> {
    let re = regex::Regex::new(r"^\[([^\]]*)\]:\s*\[(.*)\]\s*$").unwrap();
    let mut map = BTreeMap::new();
    for line in raw.lines() {
        if let Some(c) = re.captures(line.trim()) {
            map.insert(c[1].to_string(), c[2].to_string());
        }
    }
    map
}

/// Parse a `uiautomator` XML dump into a flat, index-ordered node list.
pub fn parse_ui_xml(xml: &str) -> Vec<UiNode> {
    let node_re = regex::Regex::new(r"(?s)<node\b([^>]*?)/?>").unwrap();
    let attr_re = regex::Regex::new(r#"([\w:-]+)="([^"]*)""#).unwrap();
    let bounds_re = regex::Regex::new(r"\[(-?\d+),(-?\d+)\]\[(-?\d+),(-?\d+)\]").unwrap();
    let mut nodes = Vec::new();
    for (i, m) in node_re.captures_iter(xml).enumerate() {
        let attrs = &m[1];
        let mut map = std::collections::HashMap::new();
        for a in attr_re.captures_iter(attrs) {
            map.insert(a[1].to_string(), a[2].to_string());
        }
        let bounds = map
            .get("bounds")
            .and_then(|b| bounds_re.captures(b))
            .map(|c| {
                let x1: i32 = c[1].parse().unwrap_or(0);
                let y1: i32 = c[2].parse().unwrap_or(0);
                let x2: i32 = c[3].parse().unwrap_or(0);
                let y2: i32 = c[4].parse().unwrap_or(0);
                Rect {
                    x: x1,
                    y: y1,
                    width: x2 - x1,
                    height: y2 - y1,
                }
            })
            .unwrap_or_default();
        nodes.push(UiNode {
            index: i as u32,
            text: map.get("text").cloned().unwrap_or_default(),
            content_desc: map.get("content-desc").cloned().unwrap_or_default(),
            class: map.get("class").cloned().unwrap_or_default(),
            bounds,
            clickable: map.get("clickable").map(|v| v == "true").unwrap_or(false),
            focusable: map.get("focusable").map(|v| v == "true").unwrap_or(false),
        });
    }
    nodes
}

/// Read width/height from a PNG's IHDR chunk (big-endian, bytes 16..24).
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    Some((w, h))
}

// ------------------------------------------------------------------------------------------------
// Live identity hardening: rewrite the image's build.prop in place (root + remount). This is the
// SAME mechanism as baking identity at build time, applied to a running userdebug image. It flips
// the identity tells (sdk_gphone/emu64a/the emulator fingerprint) to the profile; it deliberately
// leaves the STRUCTURAL tells (ro.hardware=ranchu, ro.boot.qemu, /vendor/etc/fstab.ranchu, the
// Goldfish sensor HAL) untouched — those come from the ranchu kernel/device-model and require the
// custom guest image + crosvm/KVM to remove. `aemulator audit` measures the before/after.
// ------------------------------------------------------------------------------------------------

impl AdbController {
    /// Restart `adbd` as root (userdebug/eng images only).
    pub fn root(&self) -> Result<(), ControlError> {
        Command::new(&self.adb)
            .arg("-s")
            .arg(&self.serial)
            .arg("root")
            .status()
            .map_err(|e| be(format!("adb root: {e}")))?;
        std::thread::sleep(Duration::from_secs(3));
        let _ = Command::new(&self.adb)
            .arg("-s")
            .arg(&self.serial)
            .arg("wait-for-device")
            .status();
        Ok(())
    }

    /// Remount the read-only partitions read-write (requires `-writable-system` at boot + root).
    /// Remount the read-only partitions read-write. Returns `Ok(true)` when the (overlayfs) remount
    /// enabled but needs a reboot before it is actually writable, `Ok(false)` when it is writable
    /// now, and `Err` on a real failure (not rooted / no `-writable-system`).
    pub fn remount(&self) -> Result<bool, ControlError> {
        let out = Command::new(&self.adb)
            .arg("-s")
            .arg(&self.serial)
            .arg("remount")
            .output()
            .map_err(|e| be(format!("adb remount: {e}")))?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let low = combined.to_lowercase();
        if low.contains("not running as root") || low.contains("no such") {
            return Err(be(format!(
                "remount failed (boot with -writable-system and be rooted): {}",
                combined.trim()
            )));
        }
        // The overlayfs path prints "Now reboot your device for settings to take effect".
        Ok(low.contains("reboot"))
    }

    /// Reboot the device and wait for `sys.boot_completed`.
    pub fn reboot_and_wait(&self, timeout: Duration) -> Result<(), ControlError> {
        Command::new(&self.adb)
            .arg("-s")
            .arg(&self.serial)
            .arg("reboot")
            .status()
            .map_err(|e| be(format!("adb reboot: {e}")))?;
        std::thread::sleep(Duration::from_secs(4));
        let _ = Command::new(&self.adb)
            .arg("-s")
            .arg(&self.serial)
            .arg("wait-for-device")
            .status();
        self.wait_for_boot(timeout)
    }

    fn pull(&self, remote: &str, local: &Path) -> Result<(), ControlError> {
        let out = Command::new(&self.adb)
            .arg("-s")
            .arg(&self.serial)
            .arg("pull")
            .arg(remote)
            .arg(local)
            .output()
            .map_err(|e| be(format!("adb pull: {e}")))?;
        if !out.status.success() {
            return Err(be(format!(
                "pull {remote}: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    fn push(&self, local: &Path, remote: &str) -> Result<(), ControlError> {
        let out = Command::new(&self.adb)
            .arg("-s")
            .arg(&self.serial)
            .arg("push")
            .arg(local)
            .arg(remote)
            .output()
            .map_err(|e| be(format!("adb push: {e}")))?;
        if !out.status.success() {
            return Err(be(format!(
                "push {remote}: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    /// Every `build.prop` across the standard partitions.
    pub fn find_build_props(&self) -> Vec<String> {
        let out = self
            .shell(&[
                "find",
                "/system",
                "/vendor",
                "/product",
                "/odm",
                "/system_ext",
                "-name",
                "build.prop",
            ])
            .unwrap_or_default();
        out.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// True for identity keys safe to rewrite — never structural/HAL-load-bearing ones.
fn is_identity_key(k: &str) -> bool {
    if k.contains("cpu.abilist") {
        return false; // must match the actual guest ABI
    }
    if k == "ro.hardware" || k.starts_with("ro.hardware.") {
        return false; // load-bearing for HAL resolution
    }
    if k.starts_with("ro.boot.") || k.starts_with("ro.kernel.") {
        return false; // sourced from bootconfig/kernel, not build.prop
    }
    k.starts_with("ro.product.")
        || matches!(
            k,
            "ro.build.fingerprint"
                | "ro.build.flavor"
                | "ro.build.description"
                | "ro.build.tags"
                | "ro.build.type"
                | "ro.build.display.id"
        )
        || (k.starts_with("ro.") && k.ends_with(".build.fingerprint"))
}

/// The identity-only subset of a profile's emitted properties.
pub fn identity_overrides(profile: &DeviceProfile) -> BTreeMap<String, String> {
    profile
        .emit_properties()
        .into_iter()
        .filter(|(k, _)| is_identity_key(k))
        .collect()
}

/// Rewrite a `build.prop`'s identity lines to `overrides`. Returns `(new_content, lines_changed)`.
pub fn rewrite_build_prop(content: &str, overrides: &BTreeMap<String, String>) -> (String, usize) {
    let mut changed = 0;
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        let t = line.trim_start();
        if !t.starts_with('#') {
            if let Some(eq) = t.find('=') {
                let key = &t[..eq];
                if let Some(v) = overrides.get(key) {
                    let newline = format!("{key}={v}");
                    if newline != line {
                        changed += 1;
                    }
                    out.push_str(&newline);
                    out.push('\n');
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, changed)
}

/// Root + remount, then rewrite the identity across every `build.prop`. Returns
/// `(files_edited, lines_changed)`. The caller reboots (see [`AdbController::reboot_and_wait`]) to
/// apply. Requires the VM booted with `-writable-system`.
pub fn harden_identity(
    ctl: &AdbController,
    profile: &DeviceProfile,
) -> Result<(usize, usize), ControlError> {
    ctl.root()?;
    // The first remount on Android 12+ enables overlayfs but is only writable after a reboot.
    if ctl.remount()? {
        ctl.reboot_and_wait(Duration::from_secs(300))?;
        ctl.root()?;
        let _ = ctl.remount()?; // now actually read-write
    }
    let overrides = identity_overrides(profile);
    let mut files_edited = 0;
    let mut lines_changed = 0;
    let tmp = std::env::temp_dir();
    for (i, remote) in ctl.find_build_props().into_iter().enumerate() {
        let local = tmp.join(format!("aemu_bp_{i}.prop"));
        if ctl.pull(&remote, &local).is_err() {
            continue;
        }
        let content = std::fs::read_to_string(&local).unwrap_or_default();
        let (newc, changed) = rewrite_build_prop(&content, &overrides);
        if changed > 0 {
            if std::fs::write(&local, &newc).is_err() {
                continue;
            }
            if ctl.push(&local, &remote).is_ok() {
                files_edited += 1;
                lines_changed += changed;
            }
        }
    }
    Ok((files_edited, lines_changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_identity_lines_only() {
        let content = "# comment\nro.product.model=sdk_gphone64_arm64\nro.hardware=ranchu\nro.product.cpu.abilist=arm64-v8a\nunrelated=1\n";
        let mut ov = BTreeMap::new();
        ov.insert("ro.product.model".to_string(), "2211133C".to_string());
        ov.insert("ro.hardware".to_string(), "qcom".to_string()); // must be ignored by the filter upstream
                                                                  // Only feed identity keys, as identity_overrides() would:
        let mut ident = BTreeMap::new();
        ident.insert("ro.product.model".to_string(), "2211133C".to_string());
        let (out, changed) = rewrite_build_prop(content, &ident);
        assert_eq!(changed, 1);
        assert!(out.contains("ro.product.model=2211133C"));
        assert!(out.contains("ro.hardware=ranchu")); // structural tell untouched
        let _ = ov;
    }

    #[test]
    fn identity_filter_excludes_structural_keys() {
        assert!(is_identity_key("ro.product.model"));
        assert!(is_identity_key("ro.build.fingerprint"));
        assert!(is_identity_key("ro.vendor.build.fingerprint"));
        assert!(!is_identity_key("ro.hardware"));
        assert!(!is_identity_key("ro.boot.qemu"));
        assert!(!is_identity_key("ro.product.cpu.abilist"));
    }

    #[test]
    fn parses_getprop_lines() {
        let raw =
            "[ro.product.model]: [sdk_gphone64_arm64]\n[ro.kernel.qemu]: [1]\n[weird]:[novalue]\n";
        let m = parse_getprop(raw);
        assert_eq!(m.get("ro.product.model").unwrap(), "sdk_gphone64_arm64");
        assert_eq!(m.get("ro.kernel.qemu").unwrap(), "1");
    }

    #[test]
    fn parses_ui_nodes_and_bounds() {
        let xml = r#"<hierarchy><node index="0" text="Hello" class="android.widget.TextView" content-desc="" clickable="true" focusable="false" bounds="[10,20][110,70]"/><node index="1" text="" class="android.widget.Button" content-desc="OK" clickable="true" focusable="true" bounds="[0,0][50,50]"/></hierarchy>"#;
        let nodes = parse_ui_xml(xml);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].text, "Hello");
        assert!(nodes[0].clickable);
        assert_eq!(
            nodes[0].bounds,
            Rect {
                x: 10,
                y: 20,
                width: 100,
                height: 50
            }
        );
        assert_eq!(nodes[0].bounds.center(), (60, 45));
        assert_eq!(nodes[1].content_desc, "OK");
    }

    #[test]
    fn reads_png_dimensions() {
        // Minimal PNG header: signature + IHDR length/type + 1080x2400.
        let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        png.extend_from_slice(&[0, 0, 0, 13]); // IHDR len
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&1080u32.to_be_bytes());
        png.extend_from_slice(&2400u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((1080, 2400)));
    }

    #[test]
    fn injectable_props_skip_baked_tells() {
        let p = aemulator_identity::profiles::reference_profile();
        let l = AndroidHvfLauncher::default();
        let props = l.injectable_props(&p);
        // We inject brand/model/fingerprint...
        assert!(props.iter().any(|(k, _)| k == "ro.product.model"));
        // ...but never the deep, image-baked tells.
        assert!(!props.iter().any(|(k, _)| k == "ro.hardware"));
    }
}
