//! # aemulator-identity
//!
//! The **`DeviceProfile`** is the single source of device-identity truth for AEmulator. It is
//! ingested from a real per-partition `build.prop` dump and emits — *atomically and coherently* —
//! the Android system-property matrix, per-partition fingerprints, bootconfig verified-boot state,
//! per-cluster MIDR register writes, and the DTB inputs. Everything downstream (the DTB crate, the
//! VMM launcher, the CI validator) reads from one `DeviceProfile` so no two surfaces can disagree —
//! **inconsistency is the #1 detector.**
//!
//! ## What is frozen vs. what implementers fill
//!
//! The public struct shapes and method **signatures** in this crate are frozen (see `SCAFFOLD.md`).
//! [`DeviceProfile::fingerprint`] and [`DeviceProfile::from_toml`] are implemented (pure/deterministic).
//! The heavier methods — [`DeviceProfile::emit_properties`], [`DeviceProfile::emit_bootconfig`],
//! [`DeviceProfile::midr_regs`], [`DeviceProfile::validate`] — carry `todo!()` bodies for the
//! identity implementer (task #2). Do not change their signatures.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod profiles;

// Re-export the detection Finding so callers of `lint_property_map` don't need a direct dep.
pub use aemulator_detection::Finding;

/// Errors from loading or materializing a [`DeviceProfile`].
#[derive(Debug, Error)]
pub enum IdentityError {
    /// Failed to read the profile file from disk.
    #[error("failed to read profile file `{path}`: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// Failed to parse the profile TOML.
    #[error("failed to parse profile TOML: {0}")]
    Toml(#[from] toml::de::Error),
}

/// A single coherence failure surfaced by [`DeviceProfile::validate`]. `validate` returns a
/// **vector** of these so one run reports every inconsistency, not just the first.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoherenceError {
    /// A declared `ro.build.fingerprint` disagrees with the recomputed `ConstructBuildFingerprint`.
    #[error("fingerprint mismatch: profile declares `{declared}` but ConstructBuildFingerprint yields `{derived}`")]
    FingerprintMismatch { declared: String, derived: String },
    /// A blocklisted virtualization string is present in an identity field.
    #[error("blocklisted virtualization string `{needle}` present in `{field}`")]
    BlocklistedString { field: String, needle: String },
    /// The IMEI TAC does not map to the claimed `Build.MODEL`.
    #[error("TAC `{tac}` does not map to model `{model}`")]
    TacModelMismatch { tac: String, model: String },
    /// The operator numeric (PLMN) is inconsistent with the IMSI MCC/MNC.
    #[error("operator numeric `{numeric}` inconsistent with MCC `{mcc}` / MNC `{mnc}`")]
    OperatorMccMncMismatch {
        numeric: String,
        mcc: String,
        mnc: String,
    },
    /// SoC ↔ board.platform ↔ MIDR ↔ abilist ↔ page-size cross-check failed.
    #[error("SoC coherence failure: {0}")]
    SocIncoherent(String),
    /// A per-cluster MIDR implementer is anomalous for the claimed SoC (e.g. Apple `0x61`).
    #[error("MIDR implementer 0x{implementer:02x} is anomalous for the claimed SoC: {detail}")]
    MidrAnomaly { implementer: u8, detail: String },
    /// Any other coherence violation.
    #[error("{0}")]
    Other(String),
}

/// Provenance + legal metadata for a profile (never emitted to the guest).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileMeta {
    /// Human-readable profile name, e.g. `"Xiaomi 13 (fuxi) / SM8550"`.
    pub name: String,
    /// Where the identity was sourced, e.g. `"dumps.tadiphone.dev/xiaomi/fuxi"`.
    pub source: String,
    /// Legal note — fabricating a specific-OEM identity requires counsel (trademark / TAC statutes).
    pub legal_note: String,
}

/// The AOSP `android.os.Build` field set. Each field is documented with the property it drives.
///
/// **Build field → property map (verbatim AOSP `Build.java`):** `BRAND=ro.product.brand`,
/// `PRODUCT=ro.product.name`, `DEVICE=ro.product.device`, `MODEL=ro.product.model`,
/// `MANUFACTURER=ro.product.manufacturer`, `BOARD=ro.product.board` (device codename, e.g.
/// `fuxi` — **not** `ro.board.platform`), `BOOTLOADER=ro.bootloader`, `HARDWARE=ro.hardware`,
/// `HOST=ro.build.host`, `USER=ro.build.user`, `TAGS=ro.build.tags`, `TYPE=ro.build.type`,
/// `ID=ro.build.id`, incremental=`ro.build.version.incremental`, release=`ro.build.version.release`,
/// sdk=`ro.build.version.sdk`, `DISPLAY=ro.build.display.id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildIdentity {
    /// `ro.product.brand`, e.g. `"Xiaomi"`.
    pub brand: String,
    /// `ro.product.name` (AOSP `PRODUCT`), e.g. `"fuxi"`.
    pub product_name: String,
    /// `ro.product.device` (AOSP `DEVICE`), e.g. `"fuxi"`.
    pub device: String,
    /// `ro.product.model`, e.g. `"2211133C"`.
    pub model: String,
    /// `ro.product.manufacturer`, e.g. `"Xiaomi"`.
    pub manufacturer: String,
    /// `ro.product.board` (AOSP `BOARD`, device codename), e.g. `"fuxi"`.
    pub board: String,
    /// `ro.bootloader`. Never `"unknown"` on a retail device.
    pub bootloader: String,
    /// `ro.hardware`. **Load-bearing** — drives `init.<hw>.rc`, `fstab.<hw>`, HAL `.so` resolution.
    pub hardware: String,
    /// `ro.build.host` (OEM build-farm host, never `abfarm-*`).
    pub host: String,
    /// `ro.build.user` (OEM build-farm user, e.g. `"se.infra"`).
    pub user: String,
    /// `ro.build.tags`. Must be `"release-keys"` (never `test-keys`/`dev-keys`).
    pub tags: String,
    /// `ro.build.type`. Must be `"user"` on a retail device.
    pub build_type: String,
    /// `ro.build.id`, e.g. `"TKQ1.220829.002"`.
    pub build_id: String,
    /// `ro.build.version.incremental`, e.g. `"V14.0.6.0.TMCCNXM"`.
    pub incremental: String,
    /// `ro.build.version.release`, e.g. `"13"`.
    pub release: String,
    /// `ro.build.version.sdk` (`Build.VERSION.SDK_INT`), e.g. `33`.
    pub sdk: u32,
    /// `ro.build.version.security_patch`, e.g. `"2023-04-01"`.
    pub security_patch: String,
    /// `ro.build.display.id`. Usually equals `build_id` on a release build.
    pub display_id: String,
}

/// One CPU cluster's identity. Real big.LITTLE devices report **different `MIDR_EL1` per cluster**;
/// a uniform MIDR across all cores is anomalous.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MidrCluster {
    /// Human-readable core name, e.g. `"Cortex-A77"`.
    pub part_name: String,
    /// `MIDR_EL1` implementer byte, e.g. `0x41` ARM, `0x51` Qualcomm. **Never `0x61` (Apple).**
    pub implementer: u8,
    /// `MIDR_EL1` part number (12-bit), e.g. `0xd4e` for Cortex-X3.
    pub part: u16,
    /// `MIDR_EL1` variant nibble.
    pub variant: u8,
    /// `MIDR_EL1` revision nibble.
    pub revision: u8,
    /// Number of physical cores in this cluster.
    pub core_count: u32,
    /// Per-cluster `cpufreq` max, in kHz (must be plausible for the claimed SoC).
    pub max_freq_khz: u32,
}

/// Qualcomm-style `socinfo` surface exposed under `/sys/devices/soc0/*`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Socinfo {
    /// `/sys/devices/soc0/family`, literal `"Snapdragon"` on QTI.
    pub family: String,
    /// `/sys/devices/soc0/machine`, e.g. `"fuxi"`.
    pub machine: String,
    /// `/sys/devices/soc0/soc_id`, e.g. `519` for SM8550.
    pub soc_id: u32,
    /// `/sys/devices/soc0/revision`, e.g. `"2.0"`.
    pub revision: String,
    /// `/sys/devices/soc0/serial_number`.
    pub serial_number: String,
    /// `foundry_id` (debug socinfo).
    pub foundry_id: u32,
    /// `chip_family` (debug socinfo).
    pub chip_family: u32,
    /// `raw_device_family` (debug socinfo).
    pub raw_device_family: u32,
    /// `raw_device_number` (debug socinfo).
    pub raw_device_number: u32,
    /// `hardware_platform`, e.g. `"MTP"` / `"QRD"`.
    pub hardware_platform: String,
    /// `pmic_model`.
    pub pmic_model: String,
}

/// SoC identity: manufacturer/model, `ro.board.platform`, per-cluster MIDR, and socinfo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SocProfile {
    /// `ro.soc.manufacturer`, e.g. `"QTI"`.
    pub soc_manufacturer: String,
    /// `ro.soc.model`, e.g. `"SM8550"`.
    pub soc_model: String,
    /// `ro.board.platform` (SoC platform, **separate** from `ro.product.board`), e.g. `"kalama"`.
    pub board_platform: String,
    /// Per-cluster CPU identity, in cluster order (prime → gold → silver).
    pub midr_clusters: Vec<MidrCluster>,
    /// Qualcomm-style socinfo (`/sys/devices/soc0/*`).
    pub socinfo: Socinfo,
}

/// ABI lists: `ro.product.cpu.abilist{,32,64}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AbiProfile {
    /// `ro.product.cpu.abilist`, e.g. `"arm64-v8a,armeabi-v7a,armeabi"`.
    pub abilist: String,
    /// `ro.product.cpu.abilist32`, e.g. `"armeabi-v7a,armeabi"`.
    pub abilist32: String,
    /// `ro.product.cpu.abilist64`, e.g. `"arm64-v8a"`.
    pub abilist64: String,
}

/// Vulkan `VkPhysicalDeviceProperties.deviceType`. **Never `Cpu`** (SwiftShader/lavapipe tell).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VkDeviceType {
    IntegratedGpu,
    DiscreteGpu,
    VirtualGpu,
    Cpu,
    #[default]
    Other,
}

/// GPU identity: GL strings + Vulkan device properties + `ro.hardware.egl`. Every field must be
/// mutually coherent (renderer ↔ vendorID ↔ deviceType ↔ driverID ↔ extension/limit set).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuProfile {
    /// `glGetString(GL_VENDOR)`, e.g. `"Qualcomm"`.
    pub gl_vendor: String,
    /// `glGetString(GL_RENDERER)`, e.g. `"Adreno (TM) 740"`. Never `"...Translator"`/`"SwiftShader"`.
    pub gl_renderer: String,
    /// `glGetString(GL_VERSION)`, e.g. `"OpenGL ES 3.2 V@0502.0"`.
    pub gl_version: String,
    /// `VkPhysicalDeviceProperties.deviceName`, e.g. `"Adreno (TM) 740"`.
    pub vk_device_name: String,
    /// `VkPhysicalDeviceProperties.vendorID`, e.g. `0x5143` (Qualcomm).
    pub vk_vendor_id: u32,
    /// `VkPhysicalDeviceProperties.deviceType`.
    pub vk_device_type: VkDeviceType,
    /// `VkPhysicalDeviceDriverProperties.driverID`, e.g. `"VK_DRIVER_ID_QUALCOMM_PROPRIETARY"`.
    pub vk_driver_id: String,
    /// `ro.hardware.egl`, e.g. `"adreno"` / `"mali"`. Never `"emulation"`/`"swiftshader"`.
    pub ro_hardware_egl: String,
}

/// One sensor entry, reachable from an unprivileged app via `SensorManager.getSensorList`.
/// Names/vendors must be **real datasheet parts** (never `"Goldfish ..."` /
/// `"The Android Open Source Project"`), with plausible range/resolution/power/min-delay.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SensorSpec {
    /// Android `Sensor.getType()` label, e.g. `"accelerometer"` / `"magnetic_field"`.
    pub sensor_type: String,
    /// `Sensor.getName()`, e.g. `"LSM6DSO Accelerometer"`.
    pub name: String,
    /// `Sensor.getVendor()`, e.g. `"STMicroelectronics"`.
    pub vendor: String,
    /// `Sensor.getMaximumRange()`.
    pub max_range: f32,
    /// `Sensor.getResolution()` (LSB quantization step).
    pub resolution: f32,
    /// `Sensor.getPower()` in mA.
    pub power: f32,
    /// `Sensor.getMinDelay()` in microseconds.
    pub min_delay: i32,
}

/// Telephony posture. **WiFi-only is the default** — it deletes the whole IMEI/IMSI/SIM surface,
/// which is normal on a retail WiFi tablet (`ro.baseband=unknown`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TelephonyProfile {
    /// No cellular radio (default). Removes telephony properties entirely.
    #[default]
    WifiOnly,
    /// A cellular device. All fields must be mutually consistent (TAC↔model, MCC/MNC↔operator).
    Cellular {
        /// `gsm.version.baseband` (`Build.getRadioVersion()`).
        baseband: String,
        /// 8-digit GSMA-allocated TAC; must map to `Build.MODEL` in a public TAC database.
        imei_tac: String,
        /// IMSI MCC (3 digits).
        imsi_mcc: String,
        /// IMSI MNC (2–3 digits).
        imsi_mnc: String,
        /// `getNetworkOperatorName()`.
        operator_name: String,
        /// `getNetworkOperator()` PLMN (MCC+MNC concatenated).
        operator_numeric: String,
    },
}

/// Verified-boot state, mirrored into `ro.boot.*` via bootconfig and read directly by many apps.
/// A locked/green device: `verifiedbootstate=green`, `flash.locked=1`, `device_state=locked`,
/// `veritymode=enforcing` — backed by **real hashtree descriptors** (never `avbtool --flags 2`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifiedBoot {
    /// `ro.boot.verifiedbootstate` ∈ `green|yellow|orange|red`.
    pub verifiedbootstate: String,
    /// `ro.boot.flash.locked` (`true` → `"1"`).
    pub flash_locked: bool,
    /// `ro.boot.vbmeta.device_state` ∈ `locked|unlocked`.
    pub device_state: String,
    /// `ro.boot.veritymode` ∈ `enforcing|eio|logging|disabled`.
    pub veritymode: String,
}

/// Network identity: a **real OEM MAC OUI** (never `52:54:00`/`08:00:27`) + the interface set a
/// real phone exposes (`wlan0`, `p2p0`, and `rmnet_data*` when cellular).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkProfile {
    /// The OUI prefix, e.g. `"F0:B4:29"` (Xiaomi). Per-instance NIC address is minted from this.
    pub mac_oui: String,
    /// Interface names present in `/proc/net/dev`, e.g. `["wlan0", "p2p0"]`.
    pub interfaces: Vec<String>,
}

/// Which AArch64 identification register a [`MidrRegWrite`] targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdReg {
    /// `MIDR_EL1` — main implementer/part/variant/revision id.
    MidrEl1,
    /// `REVIDR_EL1` — implementation-specific revision.
    RevidrEl1,
}

/// A single per-cluster identification-register override, applied via `KVM_SET_ONE_REG`
/// (needs `KVM_CAP_ARM_WRITABLE_IMP_ID_REGS`). One entry per (cluster, register).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidrRegWrite {
    /// Index into [`SocProfile::midr_clusters`] this write applies to.
    pub cluster_index: usize,
    /// Which register to write.
    pub reg: IdReg,
    /// The 64-bit value to program.
    pub value: u64,
}

/// The immutable, single-source-of-truth device identity. Every downstream surface reads from one
/// of these so no two can disagree.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceProfile {
    pub meta: ProfileMeta,
    pub build: BuildIdentity,
    pub soc: SocProfile,
    pub abi: AbiProfile,
    pub gpu: GpuProfile,
    pub sensors: Vec<SensorSpec>,
    pub telephony: TelephonyProfile,
    pub verified_boot: VerifiedBoot,
    pub network: NetworkProfile,
}

impl DeviceProfile {
    /// Load a profile from a TOML file. Missing fields fall back to their `Default` (so partial
    /// profiles parse), which is why [`validate`](Self::validate) exists as the coherence gate.
    pub fn from_toml(path: impl AsRef<Path>) -> Result<Self, IdentityError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| IdentityError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let profile: DeviceProfile = toml::from_str(&text)?;
        Ok(profile)
    }

    /// Compute `ConstructBuildFingerprint` (verbatim AOSP `property_service.cpp`):
    /// `brand/product_name/device:release/build_id/incremental:build_type/tags`.
    ///
    /// Because the fingerprint is *derived*, the CI validator can recompute and cross-check it —
    /// a mismatched declared `ro.build.fingerprint` is a one-line detector.
    pub fn fingerprint(&self) -> String {
        let b = &self.build;
        format!(
            "{}/{}/{}:{}/{}/{}:{}/{}",
            b.brand,
            b.product_name,
            b.device,
            b.release,
            b.build_id,
            b.incremental,
            b.build_type,
            b.tags,
        )
    }

    /// Materialize the Android system-property map (the 5×5 `ro.product.*` matrix, all six
    /// per-partition fingerprints, per-partition `ro.{p}.build.*` / `abilist*`, `ro.soc.*`,
    /// `ro.board.platform`, the `*_for_attestation` shadow set, GPU/EGL props, and the verified-boot
    /// `ro.boot.*` mirror). **Frozen signature — body owned by the identity implementer (task #2).**
    pub fn emit_properties(&self) -> BTreeMap<String, String> {
        let mut props = BTreeMap::new();
        let b = &self.build;
        let fp = self.fingerprint();

        // --- top-level build.* (the AOSP Build field → property map) ---
        props.insert("ro.build.id".into(), b.build_id.clone());
        props.insert("ro.build.display.id".into(), b.display_id.clone());
        props.insert("ro.build.version.incremental".into(), b.incremental.clone());
        props.insert("ro.build.version.release".into(), b.release.clone());
        // On a release build `release_or_codename` == release and the codename is the literal "REL".
        props.insert(
            "ro.build.version.release_or_codename".into(),
            b.release.clone(),
        );
        props.insert("ro.build.version.codename".into(), "REL".into());
        props.insert("ro.build.version.sdk".into(), b.sdk.to_string());
        props.insert(
            "ro.build.version.security_patch".into(),
            b.security_patch.clone(),
        );
        props.insert("ro.build.type".into(), b.build_type.clone());
        props.insert("ro.build.tags".into(), b.tags.clone());
        props.insert("ro.build.user".into(), b.user.clone());
        props.insert("ro.build.host".into(), b.host.clone());
        props.insert("ro.build.fingerprint".into(), fp.clone());
        props.insert(
            "ro.build.flavor".into(),
            format!("{}-{}", b.product_name, b.build_type),
        );
        // `ro.build.description` mirrors the fingerprint in flavor form (verbatim shape from a real
        // Samsung dump: "ms013gzt-user 4.3 JLS36C G7102ZTUANC1 release-keys").
        props.insert(
            "ro.build.description".into(),
            format!(
                "{}-{} {} {} {} {}",
                b.product_name, b.build_type, b.release, b.build_id, b.incremental, b.tags
            ),
        );
        props.insert("ro.bootloader".into(), b.bootloader.clone());
        // `ro.hardware` is load-bearing for HAL resolution (init.<hw>.rc, fstab.<hw>, <mod>.<hw>.so).
        props.insert("ro.hardware".into(), b.hardware.clone());

        // --- resolved (unqualified) product identity that `android.os.Build` reads directly ---
        props.insert("ro.product.brand".into(), b.brand.clone());
        props.insert("ro.product.name".into(), b.product_name.clone());
        props.insert("ro.product.device".into(), b.device.clone());
        props.insert("ro.product.model".into(), b.model.clone());
        props.insert("ro.product.manufacturer".into(), b.manufacturer.clone());
        // `Build.BOARD` == ro.product.board (device codename), NOT ro.board.platform.
        props.insert("ro.product.board".into(), b.board.clone());

        // --- the 5×5 per-partition identity matrix + resolution order ---
        // RO_PRODUCT_PROPS = {brand,device,manufacturer,model,name} across
        // RO_PRODUCT_PROPS_ALLOWED_SOURCES = {odm,product,vendor,system_ext,system}.
        for partition in PARTITIONS {
            props.insert(format!("ro.product.{partition}.brand"), b.brand.clone());
            props.insert(format!("ro.product.{partition}.device"), b.device.clone());
            props.insert(
                format!("ro.product.{partition}.manufacturer"),
                b.manufacturer.clone(),
            );
            props.insert(format!("ro.product.{partition}.model"), b.model.clone());
            props.insert(
                format!("ro.product.{partition}.name"),
                b.product_name.clone(),
            );
        }
        props.insert(
            "ro.product.property_source_order".into(),
            "product,odm,vendor,system_ext,system".into(),
        );

        // --- attestation-shadow set: MUST equal the primary set, or a partial spoof is instant. ---
        props.insert("ro.product.brand_for_attestation".into(), b.brand.clone());
        props.insert(
            "ro.product.name_for_attestation".into(),
            b.product_name.clone(),
        );
        props.insert("ro.product.device_for_attestation".into(), b.device.clone());
        props.insert("ro.product.model_for_attestation".into(), b.model.clone());
        props.insert(
            "ro.product.manufacturer_for_attestation".into(),
            b.manufacturer.clone(),
        );

        // --- ABI lists (top-level + the deprecated single-ABI alias) ---
        props.insert("ro.product.cpu.abilist".into(), self.abi.abilist.clone());
        props.insert(
            "ro.product.cpu.abilist32".into(),
            self.abi.abilist32.clone(),
        );
        props.insert(
            "ro.product.cpu.abilist64".into(),
            self.abi.abilist64.clone(),
        );
        if let Some(first) = self.abi.abilist.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                props.insert("ro.product.cpu.abi".into(), first.to_string());
            }
        }

        // --- per-partition build.* + per-partition fingerprint + per-partition abilist ---
        for partition in PARTITIONS {
            props.insert(format!("ro.{partition}.build.id"), b.build_id.clone());
            props.insert(format!("ro.{partition}.build.tags"), b.tags.clone());
            props.insert(format!("ro.{partition}.build.type"), b.build_type.clone());
            props.insert(
                format!("ro.{partition}.build.version.incremental"),
                b.incremental.clone(),
            );
            props.insert(
                format!("ro.{partition}.build.version.release"),
                b.release.clone(),
            );
            props.insert(
                format!("ro.{partition}.build.version.release_or_codename"),
                b.release.clone(),
            );
            props.insert(
                format!("ro.{partition}.build.version.sdk"),
                b.sdk.to_string(),
            );
            props.insert(format!("ro.{partition}.build.fingerprint"), fp.clone());
            props.insert(
                format!("ro.{partition}.product.cpu.abilist"),
                self.abi.abilist.clone(),
            );
            props.insert(
                format!("ro.{partition}.product.cpu.abilist32"),
                self.abi.abilist32.clone(),
            );
            props.insert(
                format!("ro.{partition}.product.cpu.abilist64"),
                self.abi.abilist64.clone(),
            );
        }
        // The boot image carries only its own fingerprint (6th per-partition fingerprint).
        props.insert("ro.bootimage.build.fingerprint".into(), fp.clone());

        // --- SoC identity ---
        props.insert(
            "ro.soc.manufacturer".into(),
            self.soc.soc_manufacturer.clone(),
        );
        props.insert("ro.soc.model".into(), self.soc.soc_model.clone());
        // `ro.board.platform` is the SoC platform (e.g. "kalama"), SEPARATE from ro.product.board.
        props.insert("ro.board.platform".into(), self.soc.board_platform.clone());

        // --- GPU / EGL. The GL/Vulkan strings themselves are driver-sourced (glGetString /
        //     vkGetPhysicalDeviceProperties), NOT properties — emitting them as props would itself
        //     be anomalous. Only ro.hardware.egl is a genuine property. ---
        if !self.gpu.ro_hardware_egl.is_empty() {
            props.insert("ro.hardware.egl".into(), self.gpu.ro_hardware_egl.clone());
        }

        // --- retail-device security posture ---
        props.insert("ro.secure".into(), "1".into());
        props.insert("ro.debuggable".into(), "0".into());
        props.insert("ro.adb.secure".into(), "1".into());

        // --- verified boot mirrored into ro.boot.* (init synthesizes these from bootconfig) ---
        let vb = &self.verified_boot;
        props.insert(
            "ro.boot.verifiedbootstate".into(),
            vb.verifiedbootstate.clone(),
        );
        props.insert("ro.boot.flash.locked".into(), bool_prop(vb.flash_locked));
        props.insert(
            "ro.boot.vbmeta.device_state".into(),
            vb.device_state.clone(),
        );
        props.insert("ro.boot.veritymode".into(), vb.veritymode.clone());

        // --- telephony (or the WiFi-only equivalents) ---
        match &self.telephony {
            TelephonyProfile::WifiOnly => {
                // `ro.baseband=unknown` is the normal, non-anomalous value on a retail WiFi tablet.
                props.insert("ro.baseband".into(), "unknown".into());
            }
            TelephonyProfile::Cellular {
                baseband,
                operator_name,
                operator_numeric,
                ..
            } => {
                props.insert("gsm.version.baseband".into(), baseband.clone());
                props.insert("ro.baseband".into(), baseband.clone());
                props.insert("gsm.operator.numeric".into(), operator_numeric.clone());
                props.insert("gsm.operator.alpha".into(), operator_name.clone());
                props.insert("gsm.sim.operator.numeric".into(), operator_numeric.clone());
                props.insert("gsm.sim.operator.alpha".into(), operator_name.clone());
                props.insert("gsm.sim.state".into(), "READY".into());
            }
        }

        props
    }

    /// Emit the `androidboot.*` bootconfig lines that `init` synthesizes into `ro.boot.*`
    /// (verified-boot state, serialno, hardware). **NEVER emits `androidboot.qemu`.**
    /// **Frozen signature — body owned by the identity implementer (task #2).**
    pub fn emit_bootconfig(&self) -> Vec<(String, String)> {
        let vb = &self.verified_boot;
        // `init` synthesizes `ro.boot.*` from these `androidboot.*` lines. We deliberately emit NO
        // `androidboot.qemu` — `Build.IS_EMULATOR` reads `ro.boot.qemu`, so simply never creating
        // it means the property never exists (there is nothing to "hide").
        vec![
            (
                "androidboot.verifiedbootstate".to_string(),
                vb.verifiedbootstate.clone(),
            ),
            (
                "androidboot.flash.locked".to_string(),
                bool_prop(vb.flash_locked),
            ),
            (
                "androidboot.vbmeta.device_state".to_string(),
                vb.device_state.clone(),
            ),
            ("androidboot.veritymode".to_string(), vb.veritymode.clone()),
            ("androidboot.serialno".to_string(), self.device_serialno()),
            (
                "androidboot.hardware".to_string(),
                self.build.hardware.clone(),
            ),
        ]
    }

    /// The per-cluster `MIDR_EL1`/`REVIDR_EL1` writes to program via `KVM_SET_ONE_REG`, so
    /// `/proc/cpuinfo` and `/sys/.../midr_el1` read as the claimed heterogeneous SoC.
    /// **Frozen signature — body owned by the identity implementer (task #2).**
    pub fn midr_regs(&self) -> Vec<MidrRegWrite> {
        // One `MIDR_EL1` write per cluster. Real big.LITTLE parts report a different MIDR per
        // cluster, so a uniform value would itself be anomalous; the heterogeneity comes straight
        // from `soc.midr_clusters`. `REVIDR_EL1` is left at the hardware default (real REVIDR is
        // usually 0 and writing a synthetic value risks a fresh anomaly), so we emit only MIDR.
        self.soc
            .midr_clusters
            .iter()
            .enumerate()
            .map(|(cluster_index, cluster)| MidrRegWrite {
                cluster_index,
                reg: IdReg::MidrEl1,
                value: encode_midr(cluster),
            })
            .collect()
    }

    /// The per-instance device serial (`ro.serialno` / `androidboot.serialno`). Deliberately
    /// **distinct** from the SoC serial (`/sys/devices/soc0/serial_number`) — equal device/SoC
    /// serials are an incoherence a detector could exploit. Derived deterministically from the
    /// build fingerprint so it is stable and never empty (an empty `ro.serialno` is itself a tell);
    /// the VMM may override it per instance when minting unique identifiers.
    fn device_serialno(&self) -> String {
        format!("{:016x}", fnv1a64(self.fingerprint().as_bytes()))
    }

    /// The string identity fields the coherence gate greps for blocklisted virtualization tells,
    /// as `(field-path, value)` pairs. Kept in one place so [`validate`](Self::validate) covers the
    /// full identity surface (build + SoC + GPU + sensors + network), not just what
    /// [`emit_properties`](Self::emit_properties) happens to serialize as a property.
    fn identity_fields(&self) -> Vec<(String, String)> {
        let b = &self.build;
        let s = &self.soc;
        let g = &self.gpu;
        let mut fields = vec![
            ("build.brand".to_string(), b.brand.clone()),
            ("build.product_name".to_string(), b.product_name.clone()),
            ("build.device".to_string(), b.device.clone()),
            ("build.model".to_string(), b.model.clone()),
            ("build.manufacturer".to_string(), b.manufacturer.clone()),
            ("build.board".to_string(), b.board.clone()),
            ("build.bootloader".to_string(), b.bootloader.clone()),
            ("build.hardware".to_string(), b.hardware.clone()),
            ("build.host".to_string(), b.host.clone()),
            ("build.user".to_string(), b.user.clone()),
            ("build.build_id".to_string(), b.build_id.clone()),
            ("build.incremental".to_string(), b.incremental.clone()),
            ("build.display_id".to_string(), b.display_id.clone()),
            (
                "soc.soc_manufacturer".to_string(),
                s.soc_manufacturer.clone(),
            ),
            ("soc.soc_model".to_string(), s.soc_model.clone()),
            ("soc.board_platform".to_string(), s.board_platform.clone()),
            ("soc.socinfo.family".to_string(), s.socinfo.family.clone()),
            ("soc.socinfo.machine".to_string(), s.socinfo.machine.clone()),
            (
                "soc.socinfo.hardware_platform".to_string(),
                s.socinfo.hardware_platform.clone(),
            ),
            ("gpu.gl_vendor".to_string(), g.gl_vendor.clone()),
            ("gpu.gl_renderer".to_string(), g.gl_renderer.clone()),
            ("gpu.gl_version".to_string(), g.gl_version.clone()),
            ("gpu.vk_device_name".to_string(), g.vk_device_name.clone()),
            ("gpu.vk_driver_id".to_string(), g.vk_driver_id.clone()),
            ("gpu.ro_hardware_egl".to_string(), g.ro_hardware_egl.clone()),
        ];
        for (i, sensor) in self.sensors.iter().enumerate() {
            fields.push((format!("sensors[{i}].name"), sensor.name.clone()));
            fields.push((format!("sensors[{i}].vendor"), sensor.vendor.clone()));
        }
        fields
    }

    /// Run the full coherence gate. Returns **every** violation found (not just the first):
    /// recompute the fingerprint; grep identity fields for the blocklist; cross-check
    /// TAC↔MODEL, MCC/MNC↔operator, and SoC↔board.platform↔MIDR↔abilist↔page-size.
    /// **Frozen signature — body owned by the identity implementer (task #2).**
    pub fn validate(&self) -> Result<(), Vec<CoherenceError>> {
        let mut errs = Vec::new();
        let props = self.emit_properties();

        // 1. Fingerprint self-consistency. The profile carries no separately-declared fingerprint
        //    (it is always derived), so we (a) assert the derived value is structurally well-formed
        //    — 8 non-empty tokens: brand/name/device:release/id/incremental:type/tags — and
        //    (b) guard the emit/derive invariant: every `*.build.fingerprint` we materialize must
        //    equal the recomputed one, or `emit_properties` has drifted from `fingerprint`.
        let fp = self.fingerprint();
        let tokens: Vec<&str> = fp.split(['/', ':']).collect();
        if tokens.len() != 8 || tokens.iter().any(|t| t.is_empty()) {
            errs.push(CoherenceError::Other(format!(
                "build fingerprint `{fp}` is malformed (needs 8 non-empty tokens: \
                 brand/name/device:release/id/incremental:type/tags)"
            )));
        }
        for (key, value) in &props {
            if key.ends_with(".build.fingerprint") && value != &fp {
                errs.push(CoherenceError::FingerprintMismatch {
                    declared: value.clone(),
                    derived: fp.clone(),
                });
            }
        }

        // 2. Retail posture — a genuine consumer device is a `user` build signed with
        //    `release-keys`. `test-keys`/`userdebug`/`eng` are fatal tells (framgia/RootBeer).
        if self.build.build_type != "user" {
            errs.push(CoherenceError::Other(format!(
                "ro.build.type must be `user` on a retail device, got `{}`",
                self.build.build_type
            )));
        }
        if self.build.tags != "release-keys" {
            errs.push(CoherenceError::Other(format!(
                "ro.build.tags must be `release-keys` on a retail device, got `{}`",
                self.build.tags
            )));
        }

        // 3. Blocklist grep over the full identity surface (build/SoC/GPU/sensors/network).
        for (field, value) in self.identity_fields() {
            if let Some(needle) = contains_blocklisted(&value) {
                errs.push(CoherenceError::BlocklistedString {
                    field,
                    needle: needle.to_string(),
                });
            }
        }
        // Emulator MAC OUIs are a first-class network tell (framgia/CalebFenton).
        let oui = self.network.mac_oui.to_ascii_uppercase();
        if oui.starts_with("52:54:00") || oui.starts_with("08:00:27") {
            errs.push(CoherenceError::Other(format!(
                "network.mac_oui `{}` is an emulator/VM OUI (QEMU 52:54:00 / VirtualBox 08:00:27)",
                self.network.mac_oui
            )));
        }

        // 4. SoC ↔ board.platform ↔ MIDR ↔ abilist coherence.
        if self.soc.midr_clusters.is_empty() {
            errs.push(CoherenceError::SocIncoherent(
                "no CPU clusters declared (MIDR set is empty)".into(),
            ));
        }
        if !self.abi.abilist.split(',').any(|a| a.trim() == "arm64-v8a") {
            errs.push(CoherenceError::SocIncoherent(format!(
                "ro.product.cpu.abilist `{}` does not advertise arm64-v8a (not an ARM64 device)",
                self.abi.abilist
            )));
        }
        if self.abi.abilist.to_ascii_lowercase().contains("x86") {
            errs.push(CoherenceError::SocIncoherent(format!(
                "ro.product.cpu.abilist `{}` advertises an x86 ABI — a native-bridge tell on an \
                 ARM64 device",
                self.abi.abilist
            )));
        }
        // ro.soc.model and ro.board.platform are read together by SoC-detection libraries
        // (pytorch/cpuinfo et al.); one without the other is incoherent.
        let has_model = !self.soc.soc_model.is_empty();
        let has_platform = !self.soc.board_platform.is_empty();
        if has_model != has_platform {
            errs.push(CoherenceError::SocIncoherent(format!(
                "ro.soc.model (`{}`) and ro.board.platform (`{}`) must be set together",
                self.soc.soc_model, self.soc.board_platform
            )));
        }
        // A Qualcomm SoC-model pattern must carry a Qualcomm manufacturer.
        if has_model {
            let model = &self.soc.soc_model;
            let is_qc_model = ["SM", "MSM", "APQ", "SDM", "QCS", "QCM"]
                .iter()
                .any(|prefix| model.starts_with(prefix));
            let mfr = self.soc.soc_manufacturer.to_ascii_uppercase();
            if is_qc_model && !(mfr.contains("QTI") || mfr.contains("QUALCOMM")) {
                errs.push(CoherenceError::SocIncoherent(format!(
                    "ro.soc.model `{model}` is a Qualcomm part but ro.soc.manufacturer is `{}`",
                    self.soc.soc_manufacturer
                )));
            }
        }

        // 5. MIDR anomalies. Implementer 0x61 (Apple, an HVF `-cpu host` leak) and 0x00 (a QEMU
        //    `max`-CPU leak) match no Android SoC and are instant, fatal tells.
        for cluster in &self.soc.midr_clusters {
            match cluster.implementer {
                0x61 => errs.push(CoherenceError::MidrAnomaly {
                    implementer: 0x61,
                    detail: format!(
                        "Apple implementer on cluster `{}` — an HVF `-cpu host` leak; \
                         matches no Android SoC",
                        cluster.part_name
                    ),
                }),
                0x00 => errs.push(CoherenceError::MidrAnomaly {
                    implementer: 0x00,
                    detail: format!(
                        "null implementer on cluster `{}` — a QEMU `max`-CPU leak; \
                         matches no Android SoC",
                        cluster.part_name
                    ),
                }),
                _ => {}
            }
        }

        // 6. Verified-boot coherence (ACE reads these three props directly as tamper signals).
        let vb = &self.verified_boot;
        if !matches!(
            vb.verifiedbootstate.as_str(),
            "green" | "yellow" | "orange" | "red"
        ) {
            errs.push(CoherenceError::Other(format!(
                "ro.boot.verifiedbootstate `{}` is not one of green|yellow|orange|red",
                vb.verifiedbootstate
            )));
        }
        if !matches!(vb.device_state.as_str(), "locked" | "unlocked") {
            errs.push(CoherenceError::Other(format!(
                "ro.boot.vbmeta.device_state `{}` is not locked|unlocked",
                vb.device_state
            )));
        }
        if !matches!(
            vb.veritymode.as_str(),
            "enforcing" | "eio" | "logging" | "disabled"
        ) {
            errs.push(CoherenceError::Other(format!(
                "ro.boot.veritymode `{}` is not enforcing|eio|logging|disabled",
                vb.veritymode
            )));
        }
        // green ⇒ a fully-locked chain of trust with dm-verity active.
        if vb.verifiedbootstate == "green" {
            if !vb.flash_locked || vb.device_state != "locked" {
                errs.push(CoherenceError::Other(
                    "verifiedbootstate=green requires flash.locked=1 and vbmeta.device_state=locked"
                        .into(),
                ));
            }
            if !matches!(vb.veritymode.as_str(), "enforcing" | "eio") {
                errs.push(CoherenceError::Other(format!(
                    "verifiedbootstate=green requires veritymode enforcing|eio, got `{}`",
                    vb.veritymode
                )));
            }
        }

        // 7. Telephony coherence (Cellular only — WiFi-only has no telephony surface).
        if let TelephonyProfile::Cellular {
            imei_tac,
            imsi_mcc,
            imsi_mnc,
            operator_numeric,
            ..
        } = &self.telephony
        {
            // TAC: exactly 8 numeric digits, mapping to a real make+model — never the emulator
            // defaults (IMEI 000…/012345678912345 → TAC 00000000/01234567).
            let tac_well_formed =
                imei_tac.len() == 8 && imei_tac.chars().all(|c| c.is_ascii_digit());
            let tac_is_default = imei_tac == "00000000" || imei_tac == "01234567";
            if !tac_well_formed || tac_is_default || self.build.model.is_empty() {
                errs.push(CoherenceError::TacModelMismatch {
                    tac: imei_tac.clone(),
                    model: self.build.model.clone(),
                });
            } else {
                // A well-formed TAC must yield a Luhn-valid 15-digit IMEI once a serial + check
                // digit are minted (3GPP TS 23.003 / ISO-IEC 7812).
                let body = format!("{imei_tac}000000");
                match luhn_check_digit(&body) {
                    Some(cd) if luhn_is_valid(&format!("{body}{cd}")) => {}
                    _ => errs.push(CoherenceError::TacModelMismatch {
                        tac: imei_tac.clone(),
                        model: self.build.model.clone(),
                    }),
                }
            }
            // IMSI MCC (3) + MNC (2–3) must concatenate to the operator PLMN numeric.
            let mcc_ok = imsi_mcc.len() == 3 && imsi_mcc.chars().all(|c| c.is_ascii_digit());
            let mnc_ok =
                (2..=3).contains(&imsi_mnc.len()) && imsi_mnc.chars().all(|c| c.is_ascii_digit());
            if !mcc_ok || !mnc_ok || operator_numeric != &format!("{imsi_mcc}{imsi_mnc}") {
                errs.push(CoherenceError::OperatorMccMncMismatch {
                    numeric: operator_numeric.clone(),
                    mcc: imsi_mcc.clone(),
                    mnc: imsi_mnc.clone(),
                });
            }
        }

        // 8. Detection-crate lint on the materialized property map — the SAME gate the CI validator
        //    runs (grep the emitted props against the hard blocklist). Any hit is a blocklisted
        //    string that reached a real property.
        for finding in aemulator_detection::lint_properties(&props) {
            errs.push(CoherenceError::BlocklistedString {
                field: finding.location,
                needle: finding.matched,
            });
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// Convenience: lint an already-materialized property map against the detection catalogue.
    /// Delegates to [`aemulator_detection::lint_properties`]; safe to call today (returns the empty
    /// set until the detection blocklist is populated).
    pub fn lint_property_map(props: &BTreeMap<String, String>) -> Vec<Finding> {
        aemulator_detection::lint_properties(props)
    }
}

// ---------------------------------------------------------------------------------------------
// Private helpers (free functions + constants). Adding private items is allowed by the scaffold;
// no public signature is touched here.
// ---------------------------------------------------------------------------------------------

/// The per-partition property sources, in AOSP `RO_PRODUCT_PROPS_ALLOWED_SOURCES` order. Drives
/// the 5×5 identity matrix and the per-partition `ro.{p}.build.*` / `abilist` emission.
const PARTITIONS: [&str; 5] = ["odm", "product", "vendor", "system_ext", "system"];

/// Virtualization / emulator tells that must never appear in a shipped identity field. This is a
/// crate-local grep (matched case-insensitively as a substring) that complements
/// [`aemulator_detection::lint_properties`]: it covers identity fields that are NOT serialized as
/// properties (GL strings, sensor names/vendors), so `validate` fails closed on them even before
/// the image is built. `mesa`/`translator`/`virtual` are intentionally omitted — they are
/// context-dependent (a legitimate forked-Mesa/Venus GPU identity contains "Mesa") and are the
/// property-linter's job, not a hard identity-field reject.
const VIRT_NEEDLES: &[&str] = &[
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
    "vbox86",
    "genymotion",
    "genymobile",
    "bluestacks",
    "sdk_gphone",
    "emu64a",
    "google_sdk",
    "the android open source project",
];

/// Format an Android boolean property (`ro.boot.flash.locked`, etc.) as `"1"`/`"0"`.
fn bool_prop(value: bool) -> String {
    if value { "1" } else { "0" }.to_string()
}

/// Case-insensitive substring scan of `value` against [`VIRT_NEEDLES`]; returns the first tell hit.
fn contains_blocklisted(value: &str) -> Option<&'static str> {
    let lower = value.to_ascii_lowercase();
    VIRT_NEEDLES
        .iter()
        .copied()
        .find(|needle| lower.contains(needle))
}

/// Encode a [`MidrCluster`] into a 64-bit `MIDR_EL1` value.
///
/// Layout (Arm ARM): `[31:24]` Implementer, `[23:20]` Variant, `[19:16]` Architecture (`0b1111`
/// for all ARMv8+ cores — the kernel then reports "CPU architecture: 8" by consulting the
/// `ID_AA64*` registers), `[15:4]` PartNum (12-bit), `[3:0]` Revision.
fn encode_midr(cluster: &MidrCluster) -> u64 {
    ((cluster.implementer as u64) << 24)
        | (((cluster.variant as u64) & 0xF) << 20)
        | (0xF << 16)
        | (((cluster.part as u64) & 0xFFF) << 4)
        | ((cluster.revision as u64) & 0xF)
}

/// FNV-1a 64-bit hash — used to derive a stable, plausible-looking device serial from the
/// fingerprint. Not cryptographic; only needs determinism + spread.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The Luhn check digit (ISO/IEC 7812) for a numeric `body`, doubling every second digit from the
/// right. Returns `None` if `body` is empty or non-numeric.
fn luhn_check_digit(body: &str) -> Option<u8> {
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut sum = 0u32;
    for (i, ch) in body.chars().rev().enumerate() {
        let mut d = ch.to_digit(10).unwrap_or(0);
        if i % 2 == 0 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    Some(((10 - (sum % 10)) % 10) as u8)
}

/// Whether `number` (all digits, ≥2 long) is Luhn-valid — the trailing digit is the check digit
/// over the preceding body.
fn luhn_is_valid(number: &str) -> bool {
    if number.len() < 2 || !number.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let (body, check) = number.split_at(number.len() - 1);
    let declared = check
        .chars()
        .next()
        .and_then(|c| c.to_digit(10))
        .map(|d| d as u8);
    luhn_check_digit(body) == declared
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_construct_build_fingerprint_formula() {
        let mut p = DeviceProfile::default();
        p.build.brand = "Xiaomi".into();
        p.build.product_name = "fuxi".into();
        p.build.device = "fuxi".into();
        p.build.release = "13".into();
        p.build.build_id = "TKQ1.220829.002".into();
        p.build.incremental = "V14.0.6.0.TMCCNXM".into();
        p.build.build_type = "user".into();
        p.build.tags = "release-keys".into();
        assert_eq!(
            p.fingerprint(),
            "Xiaomi/fuxi/fuxi:13/TKQ1.220829.002/V14.0.6.0.TMCCNXM:user/release-keys"
        );
    }

    #[test]
    fn reference_profile_is_coherent_by_construction() {
        let p = profiles::reference_profile();
        // The bundled reference declares no separate `ro.build.fingerprint`, so the derived value
        // is authoritative and must be a well-formed 8-token fingerprint.
        let fp = p.fingerprint();
        // ConstructBuildFingerprint = brand/product/device:release/id/incremental:type/tags → 5 `/`.
        assert_eq!(
            fp.matches('/').count(),
            5,
            "fingerprint should have 5 slashes: {fp}"
        );
        assert!(
            fp.contains(":user/"),
            "reference profile should be a user build: {fp}"
        );
        assert!(
            !p.soc.midr_clusters.is_empty(),
            "reference SoC must declare CPU clusters"
        );
        // No Apple implementer may ever appear in a shipped profile.
        assert!(
            p.soc.midr_clusters.iter().all(|c| c.implementer != 0x61),
            "MIDR implementer 0x61 (Apple) is a fatal tell"
        );
    }

    #[test]
    fn default_telephony_is_wifi_only() {
        assert_eq!(
            DeviceProfile::default().telephony,
            TelephonyProfile::WifiOnly
        );
    }

    #[test]
    fn lint_property_map_is_callable_on_a_clean_map() {
        // A clean identity map yields no findings even with the detection blocklist populated.
        let mut props = BTreeMap::new();
        props.insert("ro.product.brand".to_string(), "Xiaomi".to_string());
        assert!(DeviceProfile::lint_property_map(&props).is_empty());
        // ...and the delegation is real: a goldfish value IS caught now that detection is wired.
        let mut dirty = BTreeMap::new();
        dirty.insert("ro.hardware".to_string(), "goldfish".to_string());
        assert!(!DeviceProfile::lint_property_map(&dirty).is_empty());
    }

    // --- shared fixtures -----------------------------------------------------------------------

    /// The bundled reference, but as a coherent cellular device (for the telephony-coherence tests).
    fn cellular_reference() -> DeviceProfile {
        let mut p = profiles::reference_profile();
        p.telephony = TelephonyProfile::Cellular {
            baseband: "M3.13.4".into(),
            imei_tac: "35316609".into(), // 8-digit, non-default
            imsi_mcc: "310".into(),
            imsi_mnc: "260".into(),
            operator_name: "T-Mobile".into(),
            operator_numeric: "310260".into(),
        };
        p
    }

    // --- emit_properties -----------------------------------------------------------------------

    #[test]
    fn emit_properties_has_full_5x5_matrix() {
        let p = profiles::reference_profile();
        let props = p.emit_properties();
        let expected = [
            ("brand", p.build.brand.as_str()),
            ("device", p.build.device.as_str()),
            ("manufacturer", p.build.manufacturer.as_str()),
            ("model", p.build.model.as_str()),
            ("name", p.build.product_name.as_str()),
        ];
        for partition in PARTITIONS {
            for (slot, value) in expected {
                let key = format!("ro.product.{partition}.{slot}");
                assert_eq!(
                    props.get(&key).map(String::as_str),
                    Some(value),
                    "matrix cell {key} should be `{value}`"
                );
            }
        }
        // Exactly 25 matrix cells (5 partitions × 5 slots) — matched precisely so the product
        // partition's per-partition abilist keys (ro.product.product.cpu.*) are not miscounted.
        let slots = ["brand", "device", "manufacturer", "model", "name"];
        let cells = props
            .keys()
            .filter(|k| {
                PARTITIONS.iter().any(|p| {
                    slots
                        .iter()
                        .any(|s| k.as_str() == format!("ro.product.{p}.{s}"))
                })
            })
            .count();
        assert_eq!(cells, 25, "expected a full 5x5 identity matrix");
        assert_eq!(
            props
                .get("ro.product.property_source_order")
                .map(String::as_str),
            Some("product,odm,vendor,system_ext,system")
        );
    }

    #[test]
    fn emit_properties_has_all_six_partition_fingerprints_all_equal_derived() {
        let p = profiles::reference_profile();
        let props = p.emit_properties();
        let fp = p.fingerprint();
        for partition in [
            "system",
            "system_ext",
            "vendor",
            "product",
            "odm",
            "bootimage",
        ] {
            let key = format!("ro.{partition}.build.fingerprint");
            assert_eq!(
                props.get(&key).map(String::as_str),
                Some(fp.as_str()),
                "{key} must equal the derived ConstructBuildFingerprint"
            );
        }
        assert_eq!(
            props.get("ro.build.fingerprint").map(String::as_str),
            Some(fp.as_str())
        );
    }

    #[test]
    fn emit_properties_attestation_shadow_equals_primary() {
        let props = profiles::reference_profile().emit_properties();
        for slot in ["brand", "name", "device", "model", "manufacturer"] {
            let primary = props.get(&format!("ro.product.{slot}"));
            let shadow = props.get(&format!("ro.product.{slot}_for_attestation"));
            assert!(primary.is_some(), "primary ro.product.{slot} missing");
            assert_eq!(
                shadow, primary,
                "attestation shadow for {slot} must equal the primary"
            );
        }
    }

    #[test]
    fn emit_properties_has_soc_abi_egl_and_verified_boot() {
        let props = profiles::reference_profile().emit_properties();
        assert_eq!(
            props.get("ro.soc.manufacturer").map(String::as_str),
            Some("QTI")
        );
        assert_eq!(
            props.get("ro.soc.model").map(String::as_str),
            Some("SM8550")
        );
        assert_eq!(
            props.get("ro.board.platform").map(String::as_str),
            Some("kalama")
        );
        assert_eq!(
            props.get("ro.product.board").map(String::as_str),
            Some("fuxi")
        );
        assert_eq!(
            props.get("ro.product.cpu.abilist").map(String::as_str),
            Some("arm64-v8a,armeabi-v7a,armeabi")
        );
        assert_eq!(
            props.get("ro.hardware.egl").map(String::as_str),
            Some("adreno")
        );
        // verified boot + retail security posture
        assert_eq!(
            props.get("ro.boot.verifiedbootstate").map(String::as_str),
            Some("green")
        );
        assert_eq!(
            props.get("ro.boot.flash.locked").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            props.get("ro.boot.vbmeta.device_state").map(String::as_str),
            Some("locked")
        );
        assert_eq!(
            props.get("ro.boot.veritymode").map(String::as_str),
            Some("enforcing")
        );
        assert_eq!(props.get("ro.secure").map(String::as_str), Some("1"));
        assert_eq!(props.get("ro.debuggable").map(String::as_str), Some("0"));
        // WiFi-only posture
        assert_eq!(
            props.get("ro.baseband").map(String::as_str),
            Some("unknown")
        );
    }

    #[test]
    fn emit_properties_carry_no_blocklisted_string_and_no_qemu_key() {
        let props = profiles::reference_profile().emit_properties();
        // Crate-local grep over every emitted value.
        for (key, value) in &props {
            assert!(
                contains_blocklisted(value).is_none(),
                "emitted value for {key} contains a blocklisted tell: {value}"
            );
        }
        // No key may ever be a qemu/kernel-emulator property.
        for key in props.keys() {
            assert!(!key.contains("qemu"), "emitted a qemu property key: {key}");
            assert!(
                !key.starts_with("ro.kernel."),
                "emitted a ro.kernel.* key: {key}"
            );
        }
        // The authoritative CI gate (detection crate) must also find nothing.
        assert!(
            aemulator_detection::lint_properties(&props).is_empty(),
            "detection lint flagged the reference profile's emitted properties"
        );
    }

    #[test]
    fn emit_properties_never_leaks_profile_meta() {
        let p = profiles::reference_profile();
        let props = p.emit_properties();
        for value in props.values() {
            assert_ne!(value, &p.meta.name);
            assert_ne!(value, &p.meta.source);
            assert_ne!(value, &p.meta.legal_note);
            assert!(
                !value.contains("counsel"),
                "legal_note text leaked into a property"
            );
        }
    }

    #[test]
    fn cellular_emit_properties_has_operator_props() {
        let props = cellular_reference().emit_properties();
        assert_eq!(
            props.get("gsm.version.baseband").map(String::as_str),
            Some("M3.13.4")
        );
        assert_eq!(
            props.get("gsm.operator.numeric").map(String::as_str),
            Some("310260")
        );
        assert_eq!(
            props.get("gsm.operator.alpha").map(String::as_str),
            Some("T-Mobile")
        );
        assert_eq!(
            props.get("gsm.sim.state").map(String::as_str),
            Some("READY")
        );
    }

    // --- emit_bootconfig -----------------------------------------------------------------------

    #[test]
    fn emit_bootconfig_has_required_keys_and_never_qemu() {
        let bc = profiles::reference_profile().emit_bootconfig();
        let map: BTreeMap<_, _> = bc.iter().cloned().collect();
        assert_eq!(
            map.get("androidboot.verifiedbootstate").map(String::as_str),
            Some("green")
        );
        assert_eq!(
            map.get("androidboot.flash.locked").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            map.get("androidboot.vbmeta.device_state")
                .map(String::as_str),
            Some("locked")
        );
        assert_eq!(
            map.get("androidboot.veritymode").map(String::as_str),
            Some("enforcing")
        );
        assert_eq!(
            map.get("androidboot.hardware").map(String::as_str),
            Some("qcom")
        );
        // serialno is present, non-empty, and deterministic.
        let serial = map
            .get("androidboot.serialno")
            .expect("serialno must be emitted");
        assert!(!serial.is_empty(), "an empty ro.serialno is itself a tell");
        // NEVER androidboot.qemu (Build.IS_EMULATOR reads the derived ro.boot.qemu).
        for (key, _) in &bc {
            assert!(
                !key.contains("qemu"),
                "bootconfig must never carry a qemu key: {key}"
            );
        }
        // Deterministic across calls.
        assert_eq!(bc, profiles::reference_profile().emit_bootconfig());
    }

    // --- midr_regs -----------------------------------------------------------------------------

    #[test]
    fn midr_regs_are_per_cluster_heterogeneous_and_never_apple() {
        let p = profiles::reference_profile();
        let regs = p.midr_regs();
        assert_eq!(
            regs.len(),
            p.soc.midr_clusters.len(),
            "one MIDR write per cluster"
        );
        for (i, w) in regs.iter().enumerate() {
            assert_eq!(w.cluster_index, i);
            assert!(matches!(w.reg, IdReg::MidrEl1));
            let implementer = (w.value >> 24) as u8;
            assert_ne!(implementer, 0x61, "Apple implementer is a fatal tell");
            assert_ne!(implementer, 0x00, "null implementer is a QEMU-max tell");
        }
        // Heterogeneous: the A510 silver cluster must differ from the X3/A715/A710 clusters.
        let distinct: std::collections::BTreeSet<u64> = regs.iter().map(|w| w.value).collect();
        assert!(
            distinct.len() >= 2,
            "big.LITTLE must present >=2 distinct MIDR values"
        );
    }

    #[test]
    fn midr_encoding_matches_arm_layout() {
        // Cortex-X3 prime: implementer 0x41, variant 1, arch 0xF, part 0xd4e, rev 0 → 0x411FD4E0.
        let cluster = MidrCluster {
            part_name: "Cortex-X3".into(),
            implementer: 0x41,
            part: 0xd4e,
            variant: 1,
            revision: 0,
            core_count: 1,
            max_freq_khz: 3_187_200,
        };
        assert_eq!(encode_midr(&cluster), 0x411F_D4E0);
    }

    // --- validate: happy path + TOML round-trip ------------------------------------------------

    #[test]
    fn reference_profile_passes_validate() {
        assert_eq!(profiles::reference_profile().validate(), Ok(()));
    }

    #[test]
    fn cellular_reference_passes_validate() {
        assert_eq!(cellular_reference().validate(), Ok(()));
    }

    #[test]
    fn from_toml_loads_bundled_reference_and_matches() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("profiles/xiaomi_13.toml");
        let loaded = DeviceProfile::from_toml(&path).expect("bundled xiaomi_13.toml must parse");
        assert_eq!(
            loaded.fingerprint(),
            "Xiaomi/fuxi/fuxi:13/TKQ1.220829.002/V14.0.6.0.TMCCNXM:user/release-keys"
        );
        assert_eq!(loaded.build.brand, "Xiaomi");
        assert_eq!(loaded.soc.soc_model, "SM8550");
        assert_eq!(loaded.telephony, TelephonyProfile::WifiOnly);
        assert_eq!(loaded.validate(), Ok(()));
        // The on-disk twin must materialize the exact same property map as the literal.
        assert_eq!(
            loaded.emit_properties(),
            profiles::reference_profile().emit_properties()
        );
    }

    // --- validate: coherence failures are rejected ---------------------------------------------

    fn assert_has<F: Fn(&CoherenceError) -> bool>(errs: &[CoherenceError], pred: F, what: &str) {
        assert!(
            errs.iter().any(pred),
            "expected a {what} error, got: {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_blocklisted_identity_string() {
        let mut p = profiles::reference_profile();
        p.build.hardware = "goldfish".into();
        let errs = p
            .validate()
            .expect_err("goldfish hardware must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::BlocklistedString { needle, .. } if needle == "goldfish"),
            "BlocklistedString(goldfish)",
        );
    }

    #[test]
    fn validate_rejects_blocklisted_sensor_vendor() {
        let mut p = profiles::reference_profile();
        p.sensors[0].vendor = "The Android Open Source Project".into();
        let errs = p
            .validate()
            .expect_err("goldfish sensor vendor must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::BlocklistedString { .. }),
            "BlocklistedString(sensor vendor)",
        );
    }

    #[test]
    fn validate_rejects_apple_midr_implementer() {
        let mut p = profiles::reference_profile();
        p.soc.midr_clusters[0].implementer = 0x61;
        let errs = p.validate().expect_err("Apple MIDR must fail validate");
        assert_has(
            &errs,
            |e| {
                matches!(
                    e,
                    CoherenceError::MidrAnomaly {
                        implementer: 0x61,
                        ..
                    }
                )
            },
            "MidrAnomaly(0x61)",
        );
    }

    #[test]
    fn validate_rejects_x86_abilist() {
        let mut p = profiles::reference_profile();
        p.abi.abilist = "arm64-v8a,x86_64".into();
        let errs = p.validate().expect_err("x86 ABI must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::SocIncoherent(_)),
            "SocIncoherent(x86)",
        );
    }

    #[test]
    fn validate_rejects_soc_manufacturer_mismatch() {
        let mut p = profiles::reference_profile();
        p.soc.soc_manufacturer = "Samsung".into(); // SM8550 is a Qualcomm part
        let errs = p
            .validate()
            .expect_err("SM8550 + Samsung must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::SocIncoherent(_)),
            "SocIncoherent(mfr)",
        );
    }

    #[test]
    fn validate_rejects_test_keys_and_non_user_build() {
        let mut p = profiles::reference_profile();
        p.build.tags = "test-keys".into();
        p.build.build_type = "userdebug".into();
        let errs = p
            .validate()
            .expect_err("test-keys/userdebug must fail validate");
        assert!(
            errs.iter()
                .filter(|e| matches!(e, CoherenceError::Other(_)))
                .count()
                >= 2
        );
    }

    #[test]
    fn validate_rejects_emulator_mac_oui() {
        let mut p = profiles::reference_profile();
        p.network.mac_oui = "52:54:00".into();
        let errs = p.validate().expect_err("QEMU MAC OUI must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::Other(m) if m.contains("mac_oui")),
            "Other(mac_oui)",
        );
    }

    #[test]
    fn validate_rejects_malformed_tac() {
        let mut p = cellular_reference();
        if let TelephonyProfile::Cellular { imei_tac, .. } = &mut p.telephony {
            *imei_tac = "12345".into(); // not 8 digits
        }
        let errs = p.validate().expect_err("short TAC must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::TacModelMismatch { .. }),
            "TacModelMismatch",
        );
    }

    #[test]
    fn validate_rejects_default_emulator_tac() {
        let mut p = cellular_reference();
        if let TelephonyProfile::Cellular { imei_tac, .. } = &mut p.telephony {
            *imei_tac = "01234567".into(); // emulator default 012345678912345
        }
        let errs = p
            .validate()
            .expect_err("emulator-default TAC must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::TacModelMismatch { .. }),
            "TacModelMismatch(default)",
        );
    }

    #[test]
    fn validate_rejects_operator_mcc_mnc_mismatch() {
        let mut p = cellular_reference();
        if let TelephonyProfile::Cellular {
            operator_numeric, ..
        } = &mut p.telephony
        {
            *operator_numeric = "99999".into(); // != 310260
        }
        let errs = p
            .validate()
            .expect_err("operator/MCC-MNC mismatch must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::OperatorMccMncMismatch { .. }),
            "OperatorMccMncMismatch",
        );
    }

    #[test]
    fn validate_reports_every_error_not_just_the_first() {
        let mut p = profiles::reference_profile();
        p.build.tags = "test-keys".into(); // Other
        p.abi.abilist = "x86_64".into(); // SocIncoherent (x86 AND missing arm64-v8a)
        p.soc.midr_clusters[0].implementer = 0x61; // MidrAnomaly
        p.build.hardware = "ranchu".into(); // BlocklistedString
        let errs = p
            .validate()
            .expect_err("multiple faults must fail validate");
        assert_has(&errs, |e| matches!(e, CoherenceError::Other(_)), "Other");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::SocIncoherent(_)),
            "SocIncoherent",
        );
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::MidrAnomaly { .. }),
            "MidrAnomaly",
        );
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::BlocklistedString { .. }),
            "Blocklisted",
        );
    }

    // --- Luhn helper ---------------------------------------------------------------------------

    #[test]
    fn luhn_helpers_are_correct() {
        // 490154203237518 is a canonical valid IMEI (body 49015420323751, check 8).
        assert_eq!(luhn_check_digit("49015420323751"), Some(8));
        assert!(luhn_is_valid("490154203237518"));
        assert!(!luhn_is_valid("490154203237510"));
        assert_eq!(luhn_check_digit("not-digits"), None);
    }

    #[test]
    fn device_serialno_is_stable_nonempty_and_distinct_from_soc_serial() {
        let p = profiles::reference_profile();
        let s = p.device_serialno();
        assert_eq!(s.len(), 16);
        assert_eq!(s, p.device_serialno(), "serial must be deterministic");
        assert_ne!(
            s, p.soc.socinfo.serial_number,
            "device serial != SoC serial"
        );
    }
}
