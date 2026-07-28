//! # aemulator-detection
//!
//! The **adversary's-eye view**. This crate encodes what emulator/root/VM detectors actually probe
//! (the strazzere / framgia / CalebFenton / RootBeer / MASTG corpus plus the freeRASP/Talsec/ACE
//! probe catalogue) and provides the linters the CI merge-gate runs against **built images** and
//! against a materialized property map.
//!
//! It deliberately has **no dependency on `aemulator-identity`** — [`lint_properties`] takes a plain
//! `BTreeMap<String, String>` so the CI validator can lint a property dump from any source.
//!
//! ## Two catalogues
//!
//! - [`blocklist`] — virtualization / emulator tells that must **never** appear in our built image
//!   (the hard substrings `ranchu|goldfish|qemu|cutf|vsoc|crosvm|swiftshader|dummy-virt|houdini|
//!   exagear|generic|sdk_gphone|emu64a|vbox86|genymotion`, plus file/socket/node names, `qemu.*`
//!   properties, `Goldfish` sensor names, `SwiftShader`/`Mesa`/CPU-device GL strings, emulator /
//!   cloud-phone package + product names, and the `10.0.2.15` / `52:54:00` / `08:00:27` network
//!   tells). [`scan_dir`] and [`lint_properties`] fire on these.
//! - [`probe_catalogue`] — the *reverse* surface: the paths/props hardened apps actively **read**
//!   to unmask an emulator (the exact catalogue rnidbg hardcodes — the `su`-path list, Rockchip /
//!   HiSilicon / Qualcomm SoC probes, `/dev/__properties__`, the `/proc` + `/sys` probe set, and
//!   Tencent's `qimei36` device id). A real device answers all of these plausibly; this list is
//!   what our HALs/image must satisfy, not something to strip.
//!
//! ## Matching semantics
//!
//! A [`Signal`]'s `pattern` is a **literal substring**, matched **case-insensitively** against a
//! file path, file contents, or a property key/value (via [`regex::escape`], so metacharacters such
//! as `.` in `10.0.2.15` or `fstab.ranchu` match literally). An unparseable entry fails safe — it
//! is skipped, never panics a scan.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// One detection signal — a literal substring an adversary matches (case-insensitively), tagged
/// with the detection `category` it belongs to and the `source` corpus it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// Detection bucket, e.g. `"blocklist"`, `"file-node"`, `"build-prop"`, `"hal-lib-name"`,
    /// `"gl-string"`, `"package-product"`, `"network"`, `"root-su-path"`, `"soc-probe"`.
    pub category: String,
    /// The literal substring matched (case-insensitively) against a file path / property value /
    /// catalogue string. Documented historically as a "regex pattern"; it is treated as a literal
    /// (escaped) substring so authored entries stay human-readable and free of escaping bugs.
    pub pattern: String,
    /// Provenance, e.g. `"strazzere/anti-emulator"`, `"AOSP init.ranchu.rc"`, `"ACE"`,
    /// `"rnidbg syscalls.rs"`.
    pub source: String,
}

/// A concrete hit produced by a linter: which [`Signal`] fired, `where` it fired, and what text
/// matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The signal that matched.
    pub signal: Signal,
    /// Where it matched — a file path (with a ` (contents)` suffix for a file-content hit) or a
    /// property key.
    pub location: String,
    /// The exact text that matched the pattern.
    pub matched: String,
}

// ---------------------------------------------------------------------------------------------
// Catalogue data. `(category, pattern, source)` triples, grouped by detection bucket. All strings
// are verbatim from the research dossier / AOSP / rnidbg sources — never authored from recall.
// ---------------------------------------------------------------------------------------------

/// `(category, pattern, source)`.
type Def = (&'static str, &'static str, &'static str);

/// (a) The hard blocklist substrings. A single one of these anywhere in a built image is a full
/// compromise (the plan's `ranchu|goldfish|qemu|cutf|vsoc|crosvm|swiftshader|dummy-virt|houdini|
/// exagear` gate, extended with `generic|sdk_gphone|emu64a|vbox86|genymotion`).
const HARD_SUBSTRINGS: &[Def] = &[
    (
        "blocklist",
        "ranchu",
        "AOSP device/generic/goldfish (ranchu machine)",
    ),
    ("blocklist", "goldfish", "AOSP device/generic/goldfish"),
    ("blocklist", "qemu", "QEMU / AOSP qemu-props.cpp"),
    (
        "blocklist",
        "cutf",
        "AOSP device/google/cuttlefish (ro.hardware=cutf_cvm/cutf_ivsh)",
    ),
    (
        "blocklist",
        "vsoc",
        "AOSP cuttlefish (vsoc_arm64 = virtual system-on-chip)",
    ),
    ("blocklist", "crosvm", "crosvm host process name"),
    (
        "blocklist",
        "swiftshader",
        "AOSP goldfish vendor.mk (libEGL_swiftshader)",
    ),
    (
        "blocklist",
        "dummy-virt",
        "QEMU hw/arm/virt.c (DT root compatible linux,dummy-virt)",
    ),
    (
        "blocklist",
        "houdini",
        "Intel Houdini native-bridge (libhoudini.so)",
    ),
    (
        "blocklist",
        "exagear",
        "ExaGear native-bridge (.exagear suffixed libs, ACE)",
    ),
    (
        "blocklist",
        "generic",
        "AOSP emulator (ro.product.device=generic, framgia)",
    ),
    (
        "blocklist",
        "sdk_gphone",
        "AOSP emulator (ro.product.name=sdk_gphone64_arm64)",
    ),
    (
        "blocklist",
        "emu64a",
        "AOSP modern emulator (ro.product.device=emu64a)",
    ),
    (
        "blocklist",
        "vbox86",
        "Genymotion/VirtualBox (ro.hardware=vbox86, ro.product.name=vbox86p)",
    ),
    (
        "blocklist",
        "genymotion",
        "Genymotion (com.google.android.launcher.layouts.genymotion)",
    ),
];

/// (b) File / socket / device-node tells — the canonical list from strazzere/anti-emulator,
/// framgia, MASTG-KNOW-0031, AOSP `init.ranchu.rc`, and the Cuttlefish/ACE surfaces.
const FILE_NODE_TELLS: &[Def] = &[
    // goldfish / ranchu device nodes, pipes, sockets
    ("file-node", "/dev/qemu_pipe", "strazzere/anti-emulator"),
    ("file-node", "/dev/goldfish_pipe", "AOSP goldfish"),
    ("file-node", "/dev/goldfish_sync", "AOSP goldfish"),
    ("file-node", "/dev/goldfish_dma", "AOSP goldfish"),
    ("file-node", "/dev/socket/qemud", "strazzere/anti-emulator"),
    ("file-node", "/sys/qemu_trace", "strazzere/anti-emulator"),
    (
        "file-node",
        "/proc/tty/drivers",
        "framgia (contents contain 'goldfish')",
    ),
    // goldfish / ranchu init + fstab scripts
    ("file-node", "fstab.ranchu", "AOSP init.ranchu.rc"),
    (
        "file-node",
        "init.ranchu.rc",
        "AOSP device/generic/goldfish",
    ),
    (
        "file-node",
        "ueventd.ranchu.rc",
        "AOSP device/generic/goldfish",
    ),
    (
        "file-node",
        "init.ranchu-core.sh",
        "AOSP device/generic/goldfish",
    ),
    (
        "file-node",
        "init.ranchu-net.sh",
        "AOSP device/generic/goldfish",
    ),
    (
        "file-node",
        "init.ranchu.adb.setup.sh",
        "AOSP device/generic/goldfish",
    ),
    (
        "file-node",
        "init_ranchu_device_state.sh",
        "AOSP device/generic/goldfish",
    ),
    ("file-node", "init.goldfish.rc", "strazzere/anti-emulator"),
    (
        "file-node",
        "emulator-info.txt",
        "AOSP device/generic/goldfish",
    ),
    // goldfish binaries / libs
    ("file-node", "qemu-props", "AOSP qemu-props.cpp"),
    ("file-node", "/system/bin/qemud", "strazzere/anti-emulator"),
    (
        "file-node",
        "libc_malloc_debug_qemu.so",
        "strazzere/anti-emulator",
    ),
    // Cuttlefish / crosvm
    ("file-node", "/dev/vhost-vsock", "AOSP cuttlefish"),
    ("file-node", "cuttlefish_config.json", "AOSP cuttlefish"),
    ("file-node", "CUTTLEFISH_CONFIG_FILE", "AOSP cuttlefish"),
    ("file-node", "launch_cvd", "AOSP cuttlefish host process"),
    ("file-node", "assemble_cvd", "AOSP cuttlefish host process"),
    ("file-node", "run_cvd", "AOSP cuttlefish host process"),
    // Genymotion / Nox / Andy / x86 images
    ("file-node", "/dev/socket/genyd", "Genymotion"),
    ("file-node", "/dev/socket/baseband_genyd", "Genymotion"),
    ("file-node", "init.vbox86.rc", "Genymotion/VirtualBox"),
    ("file-node", "fstab.vbox86", "Genymotion/VirtualBox"),
    ("file-node", "ueventd.vbox86.rc", "Genymotion/VirtualBox"),
    ("file-node", "fstab.nox", "Nox"),
    ("file-node", "init.nox.rc", "Nox"),
    ("file-node", "ueventd.nox.rc", "Nox"),
    ("file-node", "fstab.andy", "Andy"),
    ("file-node", "ueventd.andy.rc", "Andy"),
    ("file-node", "ueventd.android_x86.rc", "Android-x86"),
    ("file-node", "init.ttVM_x86.rc", "TTVM x86"),
    ("file-node", "fstab.ttVM_x86", "TTVM x86"),
    // ACE-probed virtpipe / kvm / exagear nodes
    ("file-node", "/dev/virtpipe-common-syzs", "ACE"),
    ("file-node", "/dev/virtpipe-common-yyb", "ACE"),
    ("file-node", "/dev/virtpipe-sec", "ACE"),
    ("file-node", "vm_main.img", "ACE"),
    ("file-node", "/sys/module/kvm", "ACE"),
    ("file-node", "libdrm.so.exagear", "ACE"),
    ("file-node", "ld-android.so.exagear", "ACE"),
];

/// (c) Property tells — `qemu`/`ranchu`-family keys from `init.ranchu.rc` + `qemu-props.cpp`, plus
/// the x86 native-bridge props that would betray any translated-execution path.
const PROP_TELLS: &[Def] = &[
    (
        "build-prop",
        "ro.kernel.qemu",
        "AOSP qemu-props.cpp (legacy IS_EMULATOR)",
    ),
    ("build-prop", "ro.kernel.qemu.gles", "AOSP init.ranchu.rc"),
    (
        "build-prop",
        "ro.kernel.android.qemud",
        "AOSP init.ranchu.rc",
    ),
    (
        "build-prop",
        "ro.boot.qemu",
        "AOSP (Build.IS_EMULATOR reads ro.boot.qemu)",
    ),
    (
        "build-prop",
        "vendor.qemu.",
        "AOSP init.ranchu.rc (vendor.qemu.* family)",
    ),
    ("build-prop", "qemu.hw.mainkeys", "AOSP init.ranchu.rc"),
    ("build-prop", "qemu.sf.lcd_density", "AOSP init.ranchu.rc"),
    ("build-prop", "qemu.sf.fake_camera", "AOSP init.ranchu.rc"),
    ("build-prop", "qemu.adb.secure", "AOSP init.ranchu.rc"),
    (
        "build-prop",
        "init.svc.qemud",
        "AOSP init.ranchu.rc (service)",
    ),
    (
        "build-prop",
        "init.svc.qemu-props",
        "AOSP init.ranchu.rc (service)",
    ),
    // ARM-translation / native-bridge props (x86 host tell)
    (
        "build-prop",
        "ro.dalvik.vm.native.bridge",
        "Intel Houdini native-bridge",
    ),
    (
        "build-prop",
        "ro.enable.native.bridge.exec",
        "Intel Houdini native-bridge",
    ),
    (
        "build-prop",
        "ro.dalvik.vm.isa.arm",
        "native-bridge ISA map (arm=x86)",
    ),
    (
        "build-prop",
        "ro.dalvik.vm.isa.arm64",
        "native-bridge ISA map (arm64=x86_64)",
    ),
];

/// (d) Sensor-name tells — reachable from any unprivileged app via
/// `SensorManager.getSensorList` → `Sensor.getName()`/`getVendor()`
/// (AOSP `emulator/sensors/sensors_qemu.c`).
const SENSOR_TELLS: &[Def] = &[
    (
        "sensor-name",
        "Goldfish 3-axis Accelerometer",
        "AOSP sensors_qemu.c",
    ),
    (
        "sensor-name",
        "Goldfish 3-axis Magnetic field sensor",
        "AOSP sensors_qemu.c",
    ),
    (
        "sensor-name",
        "Goldfish Orientation sensor",
        "AOSP sensors_qemu.c",
    ),
    (
        "sensor-name",
        "Goldfish Temperature sensor",
        "AOSP sensors_qemu.c",
    ),
    (
        "sensor-name",
        "Goldfish Proximity sensor",
        "AOSP sensors_qemu.c",
    ),
    (
        "sensor-name",
        "Goldfish SENSORS Module",
        "AOSP sensors_qemu.c (HAL module)",
    ),
    (
        "sensor-name",
        "The Android Open Source Project",
        "AOSP sensors_qemu.c (Sensor.getVendor)",
    ),
];

/// (e) Graphics-stack tells — `glGetString(GL_RENDERER/GL_VENDOR)` + Vulkan
/// `VkPhysicalDeviceProperties.deviceType`/`deviceName` (MASTG graphics + AOSP goldfish vendor.mk).
const GL_TELLS: &[Def] = &[
    (
        "gl-string",
        "Android Emulator OpenGL ES Translator",
        "AOSP emulator GLES translator",
    ),
    (
        "gl-string",
        "Translator",
        "MASTG (renderer containing 'Translator')",
    ),
    (
        "gl-string",
        "SwiftShader",
        "MASTG graphics / AOSP goldfish vendor.mk",
    ),
    (
        "gl-string",
        "Mesa",
        "MASTG graphics (Mesa software renderer)",
    ),
    (
        "gl-string",
        "llvmpipe",
        "MASTG graphics (llvmpipe software renderer)",
    ),
    (
        "gl-string",
        "VirtualBox",
        "MASTG graphics (VirtualBox renderer)",
    ),
    (
        "gl-string",
        "Bluestacks",
        "MASTG (renderer containing 'Bluestacks')",
    ),
    (
        "gl-string",
        "VK_PHYSICAL_DEVICE_TYPE_CPU",
        "Vulkan CPU device type (SwiftShader/lavapipe)",
    ),
    // goldfish EGL/HAL library names, first-class fingerprints via /vendor/lib64/hw + /proc/self/maps
    (
        "hal-lib-name",
        "libEGL_emulation",
        "AOSP goldfish vendor.mk",
    ),
    (
        "hal-lib-name",
        "libGLESv1_CM_emulation",
        "AOSP goldfish vendor.mk",
    ),
    (
        "hal-lib-name",
        "libGLESv2_emulation",
        "AOSP goldfish vendor.mk",
    ),
    (
        "hal-lib-name",
        "libOpenglSystemCommon",
        "AOSP goldfish vendor.mk",
    ),
    (
        "hal-lib-name",
        "ro.hardware.egl=emulation",
        "AOSP init.ranchu.rc",
    ),
    // Tencent cloud-phone HAL suffix (ACE greps these verbatim)
    (
        "hal-lib-name",
        "audio.primary.tenc",
        "ACE (Tencent cloud-phone HAL)",
    ),
    (
        "hal-lib-name",
        "gatekeeper.tenc",
        "ACE (Tencent cloud-phone HAL)",
    ),
    ("hal-lib-name", "gps.tenc", "ACE (Tencent cloud-phone HAL)"),
];

/// (f) Emulator / cloud-phone / virtual-app package + product names (MASTG-KNOW-0031 + ACE's 11
/// product fingerprints + 21 virtual-app stub signatures + auto-clicker blocklist).
const PACKAGE_PRODUCT_TELLS: &[Def] = &[
    // MASTG-KNOW-0031 emulator packages
    ("package-product", "com.vphone.", "MASTG-KNOW-0031"),
    ("package-product", "com.bignox.", "MASTG-KNOW-0031 (Nox)"),
    (
        "package-product",
        "com.nox.mopen.app",
        "MASTG-KNOW-0031 (Nox)",
    ),
    ("package-product", "me.haima.", "MASTG-KNOW-0031"),
    (
        "package-product",
        "com.bluestacks",
        "MASTG-KNOW-0031 (BlueStacks)",
    ),
    ("package-product", "cn.itools.", "MASTG-KNOW-0031 (iTools)"),
    ("package-product", "com.kop.", "MASTG-KNOW-0031"),
    ("package-product", "com.kaopu.", "MASTG-KNOW-0031"),
    (
        "package-product",
        "com.microvirt.",
        "MASTG-KNOW-0031 (MEmu)",
    ),
    // ACE 11 named product fingerprints
    ("package-product", "LeiDianX86", "ACE (LDPlayer)"),
    ("package-product", "NoxX86", "ACE (Nox)"),
    ("package-product", "NeteaseX86", "ACE (MuMu)"),
    ("package-product", "XiaoYaoX86", "ACE"),
    ("package-product", "TianTianX86", "ACE"),
    ("package-product", "LuDaShiX86", "ACE"),
    ("package-product", "TencentX86", "ACE (GameLoop)"),
    ("package-product", "BlueStacksX86", "ACE"),
    ("package-product", "Win11X86", "ACE (WSA)"),
    ("package-product", "GoogleX86", "ACE"),
    // virtual-app / container stub signatures
    ("package-product", "com.lody.virtual", "ACE (VirtualApp)"),
    (
        "package-product",
        "com.lbe.parallel.intl",
        "ACE (Parallel Space)",
    ),
    ("package-product", "com.bly.dkplat", "ACE"),
    ("package-product", "com.qihoo.magic", "ACE (360 DualSpace)"),
    ("package-product", "com.vmos.pro", "ACE (VMOS)"),
    ("package-product", "com.vmos.app", "ACE (VMOS)"),
    (
        "package-product",
        "com.excelliance.dualaid",
        "ACE (Dual Space)",
    ),
    ("package-product", "com.polestar.clone", "ACE"),
    ("package-product", "info.red.virtual", "ACE"),
    // cloud-phone platform props
    (
        "package-product",
        "ro.com.cph.remote_input_method",
        "ACE (cloud-phone remote input)",
    ),
    (
        "package-product",
        "cloudmatrix",
        "ACE (ro.vendor.platform=cloudmatrix)",
    ),
    // auto-clicker packages (injected-input detection bucket)
    (
        "package-product",
        "net.aisence.Touchelper",
        "ACE (auto-clicker)",
    ),
    (
        "package-product",
        "com.scriptelf.oneclickplay",
        "ACE (auto-clicker)",
    ),
    (
        "package-product",
        "com.cyjh.mobileanjian",
        "ACE (auto-clicker)",
    ),
    ("package-product", "com.cyjh.gundam", "ACE (auto-clicker)"),
];

/// (g) Network tells — QEMU SLIRP addresses + emulator MAC OUIs (framgia / CalebFenton /
/// Cryptomathic). `.` in the IPs is matched literally via [`regex::escape`].
const NETWORK_TELLS: &[Def] = &[
    ("network", "10.0.2.15", "framgia / QEMU SLIRP guest IP"),
    ("network", "10.0.2.2", "QEMU SLIRP gateway/host alias"),
    ("network", "10.0.2.3", "QEMU SLIRP DNS alias"),
    ("network", "52:54:00", "QEMU default MAC OUI"),
    ("network", "08:00:27", "VirtualBox/Genymotion MAC OUI"),
];

// --- probe catalogue (what hardened apps READ — rnidbg's hardcoded surface) -------------------

/// The root-detection `su`-path list, verbatim from
/// `rnidbg emulator/src/linux/syscalls.rs:syscall_faccessat`.
const RNIDBG_SU_PATHS: &[Def] = &[
    ("root-su-path", "/data/local/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/data/local/bin/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/data/local/xbin/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/sbin/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/su/bin/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/system/bin/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/system/bin/.ext/su", "rnidbg syscalls.rs"),
    (
        "root-su-path",
        "/system/bin/failsafe/su",
        "rnidbg syscalls.rs",
    ),
    ("root-su-path", "/system/sd/xbin/su", "rnidbg syscalls.rs"),
    (
        "root-su-path",
        "/system/usr/we-need-root/su",
        "rnidbg syscalls.rs",
    ),
    ("root-su-path", "/system/xbin/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/cache/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/data/su", "rnidbg syscalls.rs"),
    ("root-su-path", "/dev/su", "rnidbg syscalls.rs"),
];

/// SoC-vendor probe paths rnidbg intercepts (Rockchip / HiSilicon / Qualcomm socinfo).
const RNIDBG_SOC_PROBES: &[Def] = &[
    (
        "soc-probe",
        "/proc/rk_dmabuf",
        "rnidbg syscalls.rs (Rockchip)",
    ),
    (
        "soc-probe",
        "/proc/device-tree/rockchip-suspend",
        "rnidbg syscalls.rs (Rockchip)",
    ),
    (
        "soc-probe",
        "/proc/device-tree/rockchip-system-monitor",
        "rnidbg syscalls.rs (Rockchip)",
    ),
    (
        "soc-probe",
        "/proc/mpp_service/rkvenc-core0",
        "rnidbg syscalls.rs (Rockchip)",
    ),
    (
        "soc-probe",
        "/sys/bus/platform/drivers/hisi-lpc",
        "rnidbg syscalls.rs (HiSilicon)",
    ),
    (
        "soc-probe",
        "/sys/devices/soc0/serial_number",
        "rnidbg vm.rs (Qualcomm socinfo)",
    ),
];

/// The `/proc`, `/sys`, `/dev`, and device-id probe set rnidbg hardcodes — the mmap'd property
/// trie, the SELinux context, the boot-id, and Tencent's `qimei36`.
const RNIDBG_RUNTIME_PROBES: &[Def] = &[
    (
        "property-trie",
        "/dev/__properties__",
        "rnidbg syscalls.rs (bionic property trie)",
    ),
    ("device-id", "qimei36", "rnidbg vm.rs (Tencent device id)"),
    ("proc-probe", "/proc/cpuinfo", "rnidbg syscalls.rs"),
    ("proc-probe", "/proc/meminfo", "rnidbg syscalls.rs"),
    (
        "proc-probe",
        "/proc/stat",
        "rnidbg syscalls.rs (EPERM unless caller is libc.so)",
    ),
    ("proc-probe", "/proc/self/maps", "rnidbg syscalls.rs"),
    ("proc-probe", "/proc/self/cmdline", "rnidbg vm.rs"),
    (
        "proc-probe",
        "/proc/self/attr/prev",
        "rnidbg vm.rs (SELinux context)",
    ),
    (
        "proc-probe",
        "/proc/sys/kernel/random/boot_id",
        "rnidbg syscalls.rs",
    ),
    ("dev-node-probe", "/dev/pmsg0", "rnidbg syscalls.rs"),
    ("dev-node-probe", "/dev/urandom", "rnidbg syscalls.rs"),
    (
        "runtime-probe",
        "/data/misc/zoneinfo/current/tzdata",
        "rnidbg syscalls.rs",
    ),
    (
        "runtime-probe",
        "/hmdocker",
        "rnidbg syscalls.rs (HarmonyOS/container tell)",
    ),
];

/// The hard blocklist plus every other tell that must **never** appear in a built image (file /
/// socket / node names, `qemu.*` properties, `Goldfish` sensor names, `SwiftShader`/`Mesa`/CPU-type
/// GL strings, emulator/cloud-phone package + product names, `10.0.2.15` / `52:54:00` / `08:00:27`
/// network defaults). [`scan_dir`] and [`lint_properties`] fire on this set.
pub fn blocklist() -> &'static [Signal] {
    static BLOCKLIST: OnceLock<Vec<Signal>> = OnceLock::new();
    BLOCKLIST
        .get_or_init(|| {
            let groups: [&[Def]; 7] = [
                HARD_SUBSTRINGS,
                FILE_NODE_TELLS,
                PROP_TELLS,
                SENSOR_TELLS,
                GL_TELLS,
                PACKAGE_PRODUCT_TELLS,
                NETWORK_TELLS,
            ];
            groups
                .iter()
                .flat_map(|g| g.iter().map(def_to_signal))
                .collect()
        })
        .as_slice()
}

/// The probe catalogue: the paths/props hardened apps *read* to unmask an emulator — the exact set
/// rnidbg hardcodes (root `su`-path list, Rockchip/HiSilicon/Qualcomm SoC probes,
/// `/dev/__properties__`, the `/proc` + `/sys` probe set, Tencent `qimei36`). Reference data used to
/// check that our image answers these probes the way real hardware would; **not** used by the
/// image/property linters (a real device *has* `/dev/urandom`, `/proc/cpuinfo`, and a socinfo
/// serial).
pub fn probe_catalogue() -> &'static [Signal] {
    static CATALOGUE: OnceLock<Vec<Signal>> = OnceLock::new();
    CATALOGUE
        .get_or_init(|| {
            let groups: [&[Def]; 3] = [RNIDBG_SU_PATHS, RNIDBG_SOC_PROBES, RNIDBG_RUNTIME_PROBES];
            groups
                .iter()
                .flat_map(|g| g.iter().map(def_to_signal))
                .collect()
        })
        .as_slice()
}

/// Recursively scan a built-image directory for blocklisted virtualization tells in **both** file
/// paths and file contents (the CI merge-gate: the plan's "grep the BUILT images against a hard
/// blocklist"). A path hit and a content hit are reported as separate [`Finding`]s; content is
/// scanned in overlapping chunks so a match is caught even in a multi-GB image and even when it
/// straddles a read boundary. IO errors on individual files are skipped, never fatal.
pub fn scan_dir(root: &Path) -> Vec<Finding> {
    let matchers = build_matchers(blocklist());
    if matchers.is_empty() {
        return Vec::new();
    }
    let mut files = Vec::new();
    collect_files(root, &mut files, 0);
    let mut findings = Vec::new();
    for path in &files {
        let name = path.to_string_lossy();
        let name_bytes = name.as_bytes();
        for (signal, re) in &matchers {
            if re.is_match(name_bytes) {
                findings.push(Finding {
                    signal: (*signal).clone(),
                    location: name.to_string(),
                    matched: name.to_string(),
                });
            }
        }
        scan_file_contents(path, &name, &matchers, &mut findings);
    }
    findings
}

/// Lint a materialized Android property map against the [`blocklist`]. Checks both keys and values
/// (a `ro.hardware=goldfish` tells in the value; a `vendor.qemu.*` tells in the key).
pub fn lint_properties(props: &BTreeMap<String, String>) -> Vec<Finding> {
    let matchers = build_matchers(blocklist());
    let mut findings = Vec::new();
    for (signal, re) in &matchers {
        for (key, value) in props {
            if re.is_match(key.as_bytes()) {
                findings.push(Finding {
                    signal: (*signal).clone(),
                    location: key.clone(),
                    matched: key.clone(),
                });
            } else if re.is_match(value.as_bytes()) {
                findings.push(Finding {
                    signal: (*signal).clone(),
                    location: key.clone(),
                    matched: value.clone(),
                });
            }
        }
    }
    findings
}

// ---------------------------------------------------------------------------------------------
// Private plumbing.
// ---------------------------------------------------------------------------------------------

/// Content is read in 1 MiB chunks.
const CONTENT_CHUNK: usize = 1 << 20;
/// Overlap carried between chunks so a tell straddling a read boundary is not missed. Comfortably
/// larger than the longest tell in either catalogue.
const CONTENT_OVERLAP: usize = 256;

fn def_to_signal(d: &Def) -> Signal {
    Signal {
        category: d.0.to_string(),
        pattern: d.1.to_string(),
        source: d.2.to_string(),
    }
}

/// Compile each signal into a case-insensitive, literal (escaped) byte matcher. A signal whose
/// (escaped) pattern fails to compile is skipped — the catalogue is authored data, so a bad entry
/// fails safe rather than panicking a scan.
fn build_matchers(signals: &'static [Signal]) -> Vec<(&'static Signal, regex::bytes::Regex)> {
    signals
        .iter()
        .filter_map(|s| build_matcher(&s.pattern).map(|re| (s, re)))
        .collect()
}

/// Build a case-insensitive literal-substring byte matcher for `pattern`.
fn build_matcher(pattern: &str) -> Option<regex::bytes::Regex> {
    regex::bytes::RegexBuilder::new(&regex::escape(pattern))
        .case_insensitive(true)
        .unicode(false)
        .build()
        .ok()
}

/// Chunk-read `path` and append a [`Finding`] for each matcher whose pattern appears in the
/// contents (at most one content finding per signal per file). Non-regular files and unreadable
/// files are skipped.
fn scan_file_contents(
    path: &Path,
    name: &str,
    matchers: &[(&'static Signal, regex::bytes::Regex)],
    findings: &mut Vec<Finding>,
) {
    match path.metadata() {
        Ok(m) if m.is_file() => {}
        _ => return,
    }
    let Ok(mut file) = File::open(path) else {
        return;
    };
    let mut hit = vec![false; matchers.len()];
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; CONTENT_CHUNK];
    loop {
        let read = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return,
        };
        let mut window = Vec::with_capacity(carry.len() + read);
        window.extend_from_slice(&carry);
        window.extend_from_slice(&buf[..read]);
        for (i, (signal, re)) in matchers.iter().enumerate() {
            if hit[i] {
                continue;
            }
            if let Some(m) = re.find(&window) {
                hit[i] = true;
                findings.push(Finding {
                    signal: (*signal).clone(),
                    location: format!("{name} (contents)"),
                    matched: String::from_utf8_lossy(&window[m.range()]).into_owned(),
                });
            }
        }
        let keep = window.len().min(CONTENT_OVERLAP);
        carry = window[window.len() - keep..].to_vec();
    }
}

/// Recursively collect regular-file paths under `dir` (bounded depth; IO errors are skipped).
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 64 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out, depth + 1);
        } else {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway temp directory that cleans itself up on drop — avoids a `tempfile` dev-dep.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "aemulator-detection-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
            let p = self.path.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&p, contents).expect("write file");
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn patterns(signals: &[Signal]) -> Vec<&str> {
        signals.iter().map(|s| s.pattern.as_str()).collect()
    }

    #[test]
    fn catalogues_are_populated() {
        assert!(!blocklist().is_empty(), "blocklist must be populated");
        assert!(
            !probe_catalogue().is_empty(),
            "probe catalogue must be populated"
        );
    }

    #[test]
    fn blocklist_contains_every_hard_substring() {
        let pats = patterns(blocklist());
        for expected in [
            "ranchu",
            "goldfish",
            "qemu",
            "cutf",
            "vsoc",
            "crosvm",
            "swiftshader",
            "dummy-virt",
            "houdini",
            "exagear",
            "generic",
            "sdk_gphone",
            "emu64a",
            "vbox86",
            "genymotion",
        ] {
            assert!(
                pats.contains(&expected),
                "hard blocklist missing {expected:?}"
            );
        }
    }

    #[test]
    fn blocklist_covers_each_detection_bucket() {
        // file/socket/node, property, sensor, GL, package/product, network tells all present.
        let pats = patterns(blocklist());
        assert!(pats.contains(&"/dev/qemu_pipe"));
        assert!(pats.contains(&"fstab.ranchu"));
        assert!(pats.contains(&"/dev/socket/qemud"));
        assert!(pats.contains(&"ro.kernel.qemu"));
        assert!(pats.contains(&"vendor.qemu."));
        assert!(pats.contains(&"Goldfish 3-axis Accelerometer"));
        assert!(pats.contains(&"The Android Open Source Project"));
        assert!(pats.contains(&"Android Emulator OpenGL ES Translator"));
        assert!(pats.contains(&"SwiftShader"));
        assert!(pats.contains(&"VK_PHYSICAL_DEVICE_TYPE_CPU"));
        assert!(pats.contains(&"com.bluestacks"));
        assert!(pats.contains(&"10.0.2.15"));
        assert!(pats.contains(&"52:54:00"));
        assert!(pats.contains(&"08:00:27"));
    }

    #[test]
    fn probe_catalogue_has_rnidbg_surface() {
        let pats = patterns(probe_catalogue());
        // su-path list
        assert!(pats.contains(&"/system/xbin/su"));
        assert!(pats.contains(&"/system/usr/we-need-root/su"));
        // SoC probes
        assert!(pats.contains(&"/proc/rk_dmabuf"));
        assert!(pats.contains(&"/sys/bus/platform/drivers/hisi-lpc"));
        assert!(pats.contains(&"/sys/devices/soc0/serial_number"));
        // property trie + device id + proc probes
        assert!(pats.contains(&"/dev/__properties__"));
        assert!(pats.contains(&"qimei36"));
        assert!(pats.contains(&"/proc/self/attr/prev"));
        assert!(pats.contains(&"/proc/sys/kernel/random/boot_id"));
    }

    #[test]
    fn scan_dir_flags_fstab_ranchu_file() {
        let dir = TempDir::new("fstab-ranchu");
        dir.write("vendor/etc/fstab.ranchu", b"# mount table\n");
        let findings = scan_dir(&dir.path);
        assert!(
            !findings.is_empty(),
            "a fstab.ranchu file must be flagged, got no findings"
        );
        // The path carries both the literal `fstab.ranchu` tell and the hard `ranchu` substring.
        assert!(findings.iter().any(|f| f.signal.pattern == "fstab.ranchu"));
        assert!(findings.iter().any(|f| f.signal.pattern == "ranchu"));
    }

    #[test]
    fn scan_dir_flags_blocklisted_contents() {
        let dir = TempDir::new("content");
        // A benign-looking filename whose CONTENTS carry a tell (path is clean).
        dir.write("boot/kernel.bin", b"\x00\x01 goldfish_pipe driver \x7fELF");
        let findings = scan_dir(&dir.path);
        assert!(findings
            .iter()
            .any(|f| f.location.ends_with("(contents)") && f.signal.pattern == "goldfish"));
    }

    #[test]
    fn scan_dir_flags_tell_across_chunk_boundary() {
        let dir = TempDir::new("boundary");
        // Place a tell straddling the 1 MiB chunk boundary to exercise the overlap carry.
        let mut data = vec![b'.'; CONTENT_CHUNK - 4];
        data.extend_from_slice(b"swiftshader");
        let p = dir.write("lib/blob.so", &data);
        let findings = scan_dir(p.parent().unwrap());
        assert!(findings
            .iter()
            .any(|f| f.signal.pattern == "swiftshader" && f.location.ends_with("(contents)")));
    }

    #[test]
    fn scan_dir_is_case_insensitive() {
        let dir = TempDir::new("case");
        dir.write("data.txt", b"boot arg: RO.KERNEL.QEMU=1");
        let findings = scan_dir(&dir.path);
        assert!(findings.iter().any(|f| f.signal.pattern == "qemu"));
    }

    #[test]
    fn scan_dir_clean_tree_passes() {
        let dir = TempDir::new("clean");
        dir.write(
            "vendor/etc/fstab.qcom",
            b"/dev/block/by-name/system / ext4 ro wait\n",
        );
        dir.write(
            "system/build.prop",
            b"ro.product.device=fuxi\nro.board.platform=kalama\n",
        );
        assert!(
            scan_dir(&dir.path).is_empty(),
            "clean image tree must produce no findings"
        );
    }

    #[test]
    fn scan_dir_on_missing_path_is_empty() {
        assert!(scan_dir(Path::new("/nonexistent/aemulator/path")).is_empty());
    }

    #[test]
    fn lint_properties_clean_map_passes() {
        // A minimal coherent Xiaomi 13 (fuxi / SM8550) property slice — no tells.
        let mut props = BTreeMap::new();
        for (k, v) in [
            ("ro.product.brand", "Xiaomi"),
            ("ro.product.name", "fuxi"),
            ("ro.product.device", "fuxi"),
            ("ro.product.model", "2211133C"),
            ("ro.product.manufacturer", "Xiaomi"),
            ("ro.product.board", "fuxi"),
            ("ro.board.platform", "kalama"),
            ("ro.hardware", "qcom"),
            ("ro.soc.model", "SM8550"),
            ("ro.soc.manufacturer", "QTI"),
            ("ro.build.type", "user"),
            ("ro.build.tags", "release-keys"),
            ("ro.hardware.egl", "adreno"),
            (
                "ro.build.fingerprint",
                "Xiaomi/fuxi/fuxi:13/TKQ1.220829.002/V14.0.6.0.TMCCNXM:user/release-keys",
            ),
        ] {
            props.insert(k.to_string(), v.to_string());
        }
        let findings = lint_properties(&props);
        assert!(
            findings.is_empty(),
            "clean prop map must pass, got: {findings:?}"
        );
    }

    #[test]
    fn lint_properties_flags_ro_kernel_qemu() {
        let mut props = BTreeMap::new();
        props.insert("ro.kernel.qemu".to_string(), "1".to_string());
        let findings = lint_properties(&props);
        assert!(!findings.is_empty(), "ro.kernel.qemu=1 must fail the lint");
        // Flagged on the key (both the exact `ro.kernel.qemu` tell and the `qemu` hard substring).
        assert!(findings.iter().any(|f| f.location == "ro.kernel.qemu"));
        assert!(findings
            .iter()
            .any(|f| f.signal.pattern == "ro.kernel.qemu"));
        assert!(findings.iter().any(|f| f.signal.pattern == "qemu"));
    }

    #[test]
    fn lint_properties_flags_goldfish_value() {
        let mut props = BTreeMap::new();
        props.insert("ro.hardware".to_string(), "goldfish".to_string());
        let findings = lint_properties(&props);
        assert!(findings
            .iter()
            .any(|f| f.signal.pattern == "goldfish" && f.matched == "goldfish"));
    }

    #[test]
    fn lint_properties_flags_slirp_ip_value() {
        let mut props = BTreeMap::new();
        // Dotted IP must match literally (regex::escape), not as a wildcard.
        props.insert("dhcp.eth0.ipaddress".to_string(), "10.0.2.15".to_string());
        let findings = lint_properties(&props);
        assert!(findings.iter().any(|f| f.signal.pattern == "10.0.2.15"));
    }

    #[test]
    fn build_matcher_is_literal_not_wildcard() {
        // `.` in a pattern matches a literal dot only, thanks to regex::escape.
        let re = build_matcher("10.0.2.15").expect("compiles");
        assert!(re.is_match(b"addr=10.0.2.15"));
        assert!(!re.is_match(b"addr=10x0y2z15"));
    }
}
