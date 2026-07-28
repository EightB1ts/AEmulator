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

pub mod display;
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
    /// GPU renderer ↔ vendorID ↔ deviceType ↔ driverID ↔ capability cross-check failed — including
    /// the two failures that break *rendering* rather than merely leaking: an `ro.hardware.egl`
    /// naming a `libEGL_*.so` the guest image does not ship, and a claimed capability the host
    /// provably cannot back.
    #[error("GPU coherence failure: {0}")]
    GpuIncoherent(String),
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
/// mutually coherent (renderer ↔ vendorID ↔ deviceType ↔ driverID ↔ extension/limit set) — those
/// cross-checks are **implemented**, in [`DeviceProfile::validate`] §10, and they are the reason
/// two of the fields below are deliberately *less* impressive than the part they name.
///
/// ## This block does not describe a device we can impersonate
///
/// The guest image overwrites `deviceName` (with a `"Virtio-GPU GFXStream ("` prefix), `deviceType`
/// (`VIRTUAL_GPU`) and `driverID` (`MESA_GFXSTREAM`) inside `ResourceTracker.cpp` before any app
/// sees them, so nothing here can make the device *pass* a GPU-identity check. What it can do is
/// change the answer to "who is this?" from a population of one — *this* Apple M2, carrying *this*
/// permanent 16-byte `pipelineCacheUUID` — to "an Android emulator on gfxstream". That is the whole
/// design goal, and [`GpuProfile::host_spoof`] is where it is encoded.
///
/// **A capability claim is not free.** Reporting a feature FALSE that the host has is safe;
/// reporting TRUE what the host lacks makes the guest put it in `VkDeviceCreateInfo`, and
/// `vkCreateDevice` then returns `VK_ERROR_FEATURE_NOT_PRESENT` — *no graphics at all*. So the
/// claims in this block are bounded by what the measured host can actually back
/// (`crates/aemulator-hvf/gfxstream/evidence/host-vulkan-identity.json`); see [`HOST_UNBACKABLE`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuProfile {
    /// `glGetString(GL_VENDOR)`, e.g. `"Qualcomm"`. Must agree with `vk_vendor_id` and
    /// `vk_driver_id` per [`GPU_VENDOR_TRIPLES`].
    pub gl_vendor: String,
    /// `glGetString(GL_RENDERER)`, e.g. `"Adreno (TM) 740"`. Never `"...Translator"`/`"SwiftShader"`.
    pub gl_renderer: String,
    /// `glGetString(GL_VERSION)`, e.g. `"OpenGL ES 3.1 V@0744.0"`.
    ///
    /// **Capped at [`BOOT_OPENGLES_VERSION`]** (3.1) by `validate`. GLES 3.2 requires *both*
    /// geometry and tessellation shaders; the host has tessellation but `geometryShader = false`
    /// (Apple hardware — not a porting gap, and ANGLE cannot synthesize the stage). An app that
    /// reads `GL_VERSION` 3.2 and then calls `glCreateShader(GL_GEOMETRY_SHADER)` gets nothing:
    /// a worse tell than 3.1 *and* broken rendering. A real Adreno 740 *is* a GLES 3.2 part, so
    /// this is a permanent, admitted incoherence rather than a gap to close later.
    pub gl_version: String,
    /// `VkPhysicalDeviceProperties.deviceName`, e.g. `"Adreno (TM) 740"`. Must name the same part
    /// as `gl_renderer`. The host applier writes this as the *inner* text of the guest's
    /// `"Virtio-GPU GFXStream (…)"` wrapper.
    pub vk_device_name: String,
    /// `VkPhysicalDeviceProperties.vendorID`, e.g. `0x5143` (Qualcomm). A pure identifier with no
    /// capability semantics, which is exactly why it is safe to claim. `0` = leave the host's.
    pub vk_vendor_id: u32,
    /// `VkPhysicalDeviceProperties.deviceID`. Profile-owned; `0` is the sentinel meaning **leave
    /// the host's value alone**.
    ///
    /// Deliberately `0` in the bundled reference: no verified Adreno 740 Vulkan `deviceID` exists
    /// anywhere in this repo, and a fabricated one is a fresh inconsistency against a detector
    /// holding a real dump — the same reason the plan rules out fabricating limits. A profile
    /// ingested from a real device dump should carry the real value.
    pub vk_device_id: u32,
    /// `VkPhysicalDeviceProperties.deviceType`. **Never [`VkDeviceType::Cpu`]** — `validate`
    /// rejects it (SwiftShader/lavapipe tell).
    pub vk_device_type: VkDeviceType,
    /// `VkPhysicalDeviceDriverProperties.driverID`, e.g. `"VK_DRIVER_ID_QUALCOMM_PROPRIETARY"`.
    /// Must agree with `vk_vendor_id` per [`GPU_VENDOR_TRIPLES`].
    pub vk_driver_id: String,
    /// `VkPhysicalDeviceSubgroupProperties.subgroupSize`. Profile-owned; `0` = leave the host's
    /// (32 on the measured host).
    ///
    /// `0` in the bundled reference on purpose. `subgroupSize` looks like a plain number but it is
    /// a *capability* claim: a shader that reads `gl_SubgroupSize` and gets 128 while the hardware
    /// executes 32-wide computes the wrong thing. Same asymmetry as a feature bit, so it stays at
    /// the sentinel until a profile can be paired with a host that really executes that width.
    pub vk_subgroup_size: u32,
    /// `ro.hardware.egl`. Android's EGL loader `dlopen`s **`libEGL_${ro.hardware.egl}.so`**, so this
    /// value must name an implementation the guest image actually ships — see
    /// [`EGL_IMPLEMENTATIONS_SHIPPED`]. A vendor name the image cannot back (`"adreno"`,
    /// `"mali"`) means GL disappears entirely, not that GL looks like a different vendor's.
    pub ro_hardware_egl: String,
}

impl GpuProfile {
    /// Whether this profile declares a GPU identity at all. `Default` is all-empty/all-zero — the
    /// "no GPU declared" state — and [`host_spoof`](Self::host_spoof) then yields an all-sentinel
    /// [`GpuSpoof`], i.e. exact host passthrough.
    pub fn is_set(&self) -> bool {
        self.vk_vendor_id != 0
            || !self.vk_device_name.is_empty()
            || !self.gl_renderer.is_empty()
            || !self.gl_vendor.is_empty()
            || !self.gl_version.is_empty()
    }

    /// The **entire** host-side GPU identity policy, as plain data: what the host renderer should
    /// report to the guest for this profile, on this instance.
    ///
    /// `instance_seed` is the per-VM-instance seed. Everything derived from it is stable for the
    /// lifetime of that instance (so a pipeline cache still round-trips) and different across
    /// instances (so nothing links two of them).
    ///
    /// The sentinel convention is the point: `0` / `""` / all-zero means **leave the host value
    /// alone**, so a default or empty profile yields exact passthrough and the applier's default
    /// behaviour — setter never called — is "change nothing".
    ///
    /// ## Invariant: CLEAR-bits only, never set-bits
    ///
    /// Every feature this returns is a *clear* (`clear_*: bool` = "report FALSE"). There is
    /// deliberately no way to express "report TRUE". Reporting a feature the host lacks does not
    /// make the device look better; it makes the guest put that feature in `VkDeviceCreateInfo`,
    /// KosmicKrisp answer `VK_ERROR_FEATURE_NOT_PRESENT`, and **`vkCreateDevice` fail — no
    /// graphics at all.** `geometryShader` and `wideLines` are FALSE on the measured host
    /// (`gfxstream/evidence/host-vulkan-identity.json`) and must never be forced on. This is why
    /// the safe subset of an Adreno-740 spoof is strictly smaller than an Adreno 740, and why this
    /// crate does not attempt the fuller impersonation.
    pub fn host_spoof(&self, instance_seed: u64) -> GpuSpoof {
        // An undeclared GPU is exact passthrough: no clamps, no minted UUID, no strip.
        if !self.is_set() {
            return GpuSpoof::default();
        }
        GpuSpoof {
            // Pure identifiers. No capability semantics, hence safe to claim.
            vendor_id: self.vk_vendor_id,
            device_id: self.vk_device_id,
            device_name: self.vk_device_name.clone(),
            // THE highest-value field in this struct. The host's own pipelineCacheUUID is stable
            // across reboots (measured: 1655d6f0…), which makes it a permanent, unpermissioned,
            // cross-app device identifier that survives a factory reset. Replacing it with a
            // per-instance value is what deletes that identifier.
            pipeline_cache_uuid: instance_uuid(instance_seed, UUID_KIND_PIPELINE_CACHE, 0),
            // ---- FEATURE CLAMPS: CLEAR-BITS ONLY. Read the invariant above before adding one. ---
            // Both of these are TRUE on the host and FALSE on every real mobile GPU, so clearing
            // them is in the safe direction: the guest simply does not request them.
            //   * textureCompressionBC — no mobile GPU has BC/DXT. (ETC2/ASTC stay as they are.)
            //   * multiViewport        — paired with max_viewports = 1 below, NOT optional: the
            //     Vulkan spec REQUIRES maxViewports == 1 when multiViewport is unsupported, so
            //     clearing one without the other yields an internally invalid device — a louder
            //     tell than the honest 16.
            // NEVER add geometryShader or wideLines here in the other direction: they are FALSE on
            // this host, so a *set*-bit for either is total loss of graphics.
            clear_texture_compression_bc: true,
            clear_multi_viewport: true,
            max_viewports: 1,
            subgroup_size: self.vk_subgroup_size,
            strip_extensions: true,
        }
    }
}

/// What the host renderer should report to the guest in place of the real host GPU's identity —
/// plain data, no FFI types, no `unsafe`, no `libc`. Produced by [`GpuProfile::host_spoof`];
/// consumed by the applier inside the host renderer, which is a dumb setter with no policy of its
/// own.
///
/// **Sentinel convention:** `0` / `""` / all-zero / `false` all mean *leave the host's value
/// alone*. [`Default`] is therefore exact passthrough, and so is an applier whose setter is never
/// called — the two agree by construction, which is the property that makes this safe to land
/// ahead of the host side.
///
/// **Feature invariant:** every feature field is a `clear_*` flag. See
/// [`GpuProfile::host_spoof`] for why a set-bit is unrepresentable here on purpose.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GpuSpoof {
    /// `VkPhysicalDeviceProperties.vendorID`, or `0` to keep the host's.
    pub vendor_id: u32,
    /// `VkPhysicalDeviceProperties.deviceID`, or `0` to keep the host's.
    pub device_id: u32,
    /// The *inner* text of `VkPhysicalDeviceProperties.deviceName` — the guest wraps whatever the
    /// host reports in `"Virtio-GPU GFXStream (…)"`. Empty to keep the host's.
    pub device_name: String,
    /// `VkPhysicalDeviceProperties.pipelineCacheUUID`, per-instance-stable and **not** the host's.
    /// All-zero to keep the host's.
    pub pipeline_cache_uuid: [u8; 16],
    /// Report `VkPhysicalDeviceFeatures::textureCompressionBC` as FALSE (host: TRUE; no mobile GPU
    /// has BC/DXT).
    pub clear_texture_compression_bc: bool,
    /// Report `VkPhysicalDeviceFeatures::multiViewport` as FALSE (host: TRUE). **Must** be applied
    /// together with `max_viewports == 1`.
    pub clear_multi_viewport: bool,
    /// `VkPhysicalDeviceLimits::maxViewports`, or `0` to keep the host's. The spec requires this to
    /// be exactly 1 whenever `multiViewport` is unsupported, so it is always `1` when
    /// `clear_multi_viewport` is set.
    pub max_viewports: u32,
    /// `VkPhysicalDeviceSubgroupProperties::subgroupSize`, or `0` to keep the host's.
    pub subgroup_size: u32,
    /// Strip the host-only device extensions from the guest-visible list. The *names* live with the
    /// applier, not here: they describe the host driver (Metal/Apple-specific entry points), not
    /// the device this profile claims.
    pub strip_extensions: bool,
}

impl GpuSpoof {
    /// Whether this spoof is entirely sentinels, i.e. exact host passthrough. A caller may skip the
    /// applier call altogether when this is true.
    pub fn is_passthrough(&self) -> bool {
        *self == GpuSpoof::default()
    }

    /// Every Vulkan feature bit this spoof touches, as `(feature_name, reported_value)`.
    ///
    /// Exists so the CLEAR-bits-only invariant is *mechanically* checkable (see the
    /// `host_spoof_only_ever_clears_features_never_sets_them` test) rather than only asserted in a
    /// comment: every pair this returns must have `reported_value == false`, for every profile.
    pub fn feature_overrides(&self) -> Vec<(&'static str, bool)> {
        let mut overrides = Vec::new();
        if self.clear_texture_compression_bc {
            overrides.push(("textureCompressionBC", false));
        }
        if self.clear_multi_viewport {
            overrides.push(("multiViewport", false));
        }
        overrides
    }
}

/// Mint a per-instance, RFC-4122-v4-*shaped* 128-bit identifier: deterministic in
/// `(seed, kind, index)`, and different for any change to any of the three.
///
/// `seed` is the per-VM-instance seed, `kind` one of the `UUID_KIND_*` constants (domain
/// separation, so the pipeline-cache UUID and a virtio-gpu resource UUID minted from the same seed
/// cannot collide), and `index` distinguishes members of one kind (e.g. resource id).
///
/// The v4 *shape* — byte 6 high nibble `0x4`, byte 8 top bits `0b10` — is what a UUID consumer
/// checks; the value is derived, not random, which is the whole point (a random UUID could not be
/// reproduced for the same instance, and a pipeline cache would miss on every launch). Built from
/// the crate's `fnv1a64` over two domain-separated halves: not cryptographic, and not required to
/// be — it needs determinism, spread, and to carry **no host-derived bits**.
///
/// Public because the virtio-gpu device (`RESOURCE_ASSIGN_UUID`) needs exactly this function: the
/// `b"AEMU"`-plus-zero-padding magic it writes today is both a literal product name on the wire and
/// structurally anomalous (eight zero bytes in the middle of a UUID).
pub fn instance_uuid(seed: u64, kind: u32, index: u32) -> [u8; 16] {
    let mut payload = [0u8; 16];
    payload[..8].copy_from_slice(&seed.to_le_bytes());
    payload[8..12].copy_from_slice(&kind.to_le_bytes());
    payload[12..].copy_from_slice(&index.to_le_bytes());

    // Two independently domain-separated 64-bit halves: FNV-1a is a 64-bit hash, and hashing the
    // same payload twice under different prefixes is what makes the halves differ.
    let hi = fnv1a64(&[b"aemulator/instance-uuid/hi/".as_slice(), &payload].concat());
    let lo = fnv1a64(&[b"aemulator/instance-uuid/lo/".as_slice(), &payload].concat());

    let mut uuid = [0u8; 16];
    uuid[..8].copy_from_slice(&hi.to_be_bytes());
    uuid[8..].copy_from_slice(&lo.to_be_bytes());
    uuid[6] = (uuid[6] & 0x0F) | 0x40; // version 4
    uuid[8] = (uuid[8] & 0x3F) | 0x80; // variant RFC 4122
    uuid
}

/// [`instance_uuid`] domain: `VkPhysicalDeviceProperties::pipelineCacheUUID`.
pub const UUID_KIND_PIPELINE_CACHE: u32 = 1;
/// [`instance_uuid`] domain: a virtio-gpu `RESOURCE_ASSIGN_UUID` reply (`index` = resource id).
pub const UUID_KIND_RESOURCE: u32 = 2;
/// [`instance_uuid`] domain: `VkPhysicalDeviceIDProperties::deviceUUID`. Reserved — the guest image
/// zeroes this field itself today, so writing it is currently dead code.
pub const UUID_KIND_DEVICE: u32 = 3;
/// [`instance_uuid`] domain: `VkPhysicalDeviceIDProperties::driverUUID`. Reserved, same reason as
/// [`UUID_KIND_DEVICE`].
pub const UUID_KIND_DRIVER: u32 = 4;

// ---------------------------------------------------------------------------------------------
// GPU policy tables. Public because they ARE the policy: the host renderer must not carry a second,
// drifting copy of any of it, and CI runs this crate's tests on ubuntu-latest while the VMM crate
// is workspace-excluded.
// ---------------------------------------------------------------------------------------------

/// The `androidboot.opengles.version` value every AEmulator launcher puts on the guest kernel
/// command line: `(3 << 16) | 1` = **GLES 3.1**, the ceiling this host can honestly back.
///
/// Named here so [`GpuProfile::gl_version`] and the boot cmdline cannot drift: `validate` pins the
/// claimed `GL_VERSION` to at most this. Raising it to `196610` (3.2) is explicitly the wrong fix —
/// GLES 3.2 mandates geometry shaders, the host has none, and an app that reads 3.2 and then calls
/// `glCreateShader(GL_GEOMETRY_SHADER)` is both a worse tell and broken rendering.
///
/// The launchers are shell scripts and cannot import a Rust constant; a text-scan pin test over
/// them is tracked separately (plan item 4.11).
pub const BOOT_OPENGLES_VERSION: u32 = 196_609;

/// The EGL implementations the guest image actually ships as `libEGL_<name>.so`, and therefore the
/// only legal values of [`GpuProfile::ro_hardware_egl`].
///
/// Android's EGL loader `dlopen`s `libEGL_${ro.hardware.egl}.so` and has **no fallback to a vendor
/// name it cannot find** — a wrong value is not "GL that looks like someone else's", it is *no GL*,
/// surfacing late (SurfaceFlinger/zygote abort) and silently from the host's point of view. So this
/// is an allowlist of what the image can load, not a wishlist of vendors:
///
/// * `angle` — the ANGLE-over-Vulkan driver, present in the image and verified to initialize
///   (`renderer: ANGLE (Vulkan 1.3.0 …) … OpenGL ES 3.1.0`). **Not a tell**: Pixel ships ANGLE as
///   its default GLES driver, so a real retail device reports exactly this.
/// * `emulation` — the goldfish `libEGL_emulation.so`, also present (a boot with
///   `egl=emulation` got as far as *transport* failure, i.e. the library loaded). Loadable, but a
///   goldfish tell in its own right, so it is not what any profile should choose.
///
/// A vendor name the image has no `libEGL_` for (`adreno`, `mali`, `powervr`) is rejected by
/// `validate`, which is the check that would have caught the `"adreno"` this crate used to emit.
pub const EGL_IMPLEMENTATIONS_SHIPPED: &[&str] = &["angle", "emulation"];

/// One coherent mobile-GPU vendor identity: the `vendorID` ↔ `GL_VENDOR` ↔ `driverID` triple that a
/// real device reports together, plus the part family its marketing names use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuVendorTriple {
    /// `VkPhysicalDeviceProperties.vendorID`.
    pub vk_vendor_id: u32,
    /// Case-insensitive needle the `glGetString(GL_VENDOR)` string must contain.
    pub gl_vendor: &'static str,
    /// The exact `VkPhysicalDeviceDriverProperties.driverID` enum name.
    pub vk_driver_id: &'static str,
    /// Case-insensitive needle the renderer / device name must contain, e.g. `"Adreno"`.
    pub part_family: &'static str,
}

/// The vendor triples a profile may claim. Fail-closed: a `vk_vendor_id` absent from this table is
/// rejected, because the table is the *only* thing that can keep the three fields agreeing — a
/// detector reads all three and one mismatched pair is a one-line detection. Extend it (with the
/// matching `GL_VENDOR` and `driverID`) rather than working around it.
pub const GPU_VENDOR_TRIPLES: &[GpuVendorTriple] = &[
    GpuVendorTriple {
        vk_vendor_id: 0x5143,
        gl_vendor: "Qualcomm",
        vk_driver_id: "VK_DRIVER_ID_QUALCOMM_PROPRIETARY",
        part_family: "Adreno",
    },
    GpuVendorTriple {
        vk_vendor_id: 0x13b5,
        gl_vendor: "ARM",
        vk_driver_id: "VK_DRIVER_ID_ARM_PROPRIETARY",
        part_family: "Mali",
    },
    GpuVendorTriple {
        vk_vendor_id: 0x1010,
        gl_vendor: "Imagination",
        vk_driver_id: "VK_DRIVER_ID_IMAGINATION_PROPRIETARY",
        part_family: "PowerVR",
    },
    GpuVendorTriple {
        vk_vendor_id: 0x144d,
        gl_vendor: "Samsung",
        vk_driver_id: "VK_DRIVER_ID_SAMSUNG_PROPRIETARY",
        part_family: "Xclipse",
    },
];

/// A capability the **measured host** provably cannot provide, together with the profile claims that
/// would imply it.
///
/// See [`HOST_UNBACKABLE`] for why this exists at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostUnbackable {
    /// The capability, named as the host reports it (a `VkPhysicalDeviceFeatures` field or an
    /// extension name).
    pub capability: &'static str,
    /// Why the host cannot back it, quoted from the measured baseline.
    pub host_evidence: &'static str,
    /// A GLES 3.x claim at or above this minor version implies the capability. `None` when no GLES
    /// level implies it.
    pub implied_by_gles_3_minor: Option<u32>,
    /// Case-insensitive needles which, appearing anywhere in the profile's GPU claim strings, imply
    /// the capability.
    pub claim_needles: &'static [&'static str],
}

/// Capabilities the host **provably** lacks, per
/// `crates/aemulator-hvf/gfxstream/evidence/host-vulkan-identity.json` (a reproducible dump of the
/// real host driver, deliberately byte-stable across reruns).
///
/// A profile that *claims* one of these is not merely over-stating the device: the guest will
/// request it, the host will refuse, and rendering breaks. This is the rule that would have caught
/// the GLES 3.2 claim this crate shipped before Phase 4.2.
///
/// Only capabilities a [`GpuProfile`] can actually claim are listed. `wideLines`,
/// `fillModeNonSolid`, `depthBounds`, `shaderFloat64` and `pipelineStatisticsQuery` are also FALSE
/// on this host, but nothing in `GpuProfile` can assert them, so an entry for them would be
/// vacuous — and a vacuous rule that reads as coverage is worse than no rule.
pub const HOST_UNBACKABLE: &[HostUnbackable] = &[
    HostUnbackable {
        capability: "geometryShader",
        host_evidence: "features.geometryShader = false — Apple hardware has no geometry stage, \
                        and ANGLE cannot synthesize one (tessellationShader IS true, so GLES 3.2 \
                        fails on the geometry half alone)",
        implied_by_gles_3_minor: Some(2),
        claim_needles: &["geometry_shader", "geometryshader"],
    },
    HostUnbackable {
        capability: "VK_KHR_ray_query",
        host_evidence: "absent from all 140 host device extensions (no ray-query, ray-tracing \
                        pipeline, or acceleration-structure extension is present)",
        implied_by_gles_3_minor: None,
        claim_needles: &["ray_query", "rayquery", "ray_tracing", "raytracing"],
    },
];

/// The physical panel. **Screen metrics are first-order fingerprint payload** — every app reads
/// them unpermissioned via `DisplayMetrics`/`WindowMetrics`, and a 4:3 1024×768 landscape panel on a
/// device claiming a Xiaomi 13 is a one-line detector. This block is the single source of truth for
/// all of it: the virtio-gpu scanout geometry, the EDID the guest's DRM connector parses
/// ([`DisplayProfile::edid`]), `ro.sf.lcd_density`, and `androidboot.lcd_density`.
///
/// The *physical* size is deliberately **not** a field: panels are specced by diagonal, so
/// `diagonal_mm` is stored and the millimetre extents are derived ([`DisplayProfile::physical_mm`]).
/// A stored width/height in mm could drift out of agreement with the resolution; a derived one
/// cannot.
///
/// [`Default`] is all-zero (like every other block here), which means "no panel declared" —
/// [`DisplayProfile::is_set`] is what consumers branch on, and a zeroed block emits no properties,
/// no bootconfig line, and is skipped by [`DeviceProfile::validate`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayProfile {
    /// Native panel width in pixels, in the panel's native (portrait) orientation, e.g. `1080`.
    /// Drives `DisplayMetrics.widthPixels` / `wm size`, the DRM preferred mode's `hdisplay`, and the
    /// virtio-gpu scanout width. Must be a multiple of 8 — the EDID detailed timing's horizontal
    /// values are CVT cell-granular, and the guest kernel rejects a preferred mode that disagrees
    /// with the scanout by more than 16 px.
    pub width_px: u32,
    /// Native panel height in pixels, e.g. `2400`. Drives `DisplayMetrics.heightPixels`, the DRM
    /// mode's `vdisplay`, and the virtio-gpu scanout height.
    pub height_px: u32,
    /// The *declared* density bucket: `ro.sf.lcd_density` / `androidboot.lcd_density` →
    /// `DisplayMetrics.densityDpi` / `density` / `wm density`, e.g. `440`. This is a real device's
    /// **rounded** density, not its physical dpi — a real `fuxi` reports 440 against a ~414 ppi
    /// panel (yielding 393 dp of width), and `validate` bounds how far the two may diverge.
    /// **Never** emitted as `qemu.sf.lcd_density`: SurfaceFlinger prefers that key *and* it is on
    /// our own detection blocklist.
    pub density_dpi: u32,
    /// Panel refresh rate in Hz, e.g. `60`. Drives the EDID detailed timing's field rate (and hence
    /// the DRM mode's `vrefresh` and `Display.getRefreshRate()`), plus the range-limits descriptor's
    /// vertical window.
    pub refresh_hz: u32,
    /// Panel diagonal in millimetres, e.g. `162` (6.36″). The one physical dimension real panels are
    /// specced by; [`physical_mm`](Self::physical_mm) derives the EDID's `width_mm`/`height_mm` from
    /// it, which is what the guest's DRM connector reports and Android turns into `xdpi`/`ydpi` and
    /// the internal display's `DisplayViewport` (the precondition for `INPUT_PROP_DIRECT` touch).
    pub diagonal_mm: u32,
}

impl DisplayProfile {
    /// The active area in millimetres, **derived** from the pixel aspect ratio and `diagonal_mm`:
    /// `mm = px × diagonal_mm / hypot(width_px, height_px)`. These are the values the EDID carries
    /// and therefore what the guest's DRM connector reports as `width_mm`/`height_mm`.
    ///
    /// Truncates rather than rounds, so the advertised active area is never *larger* than the panel
    /// (1080×2400 @ 162 mm → 66×147 mm; rounding would claim 148 mm of height for a 147.7 mm panel).
    /// Returns `(0, 0)` for an unset block; [`display::build_edid`] then clamps the millimetre bytes
    /// it writes to at least 1, because a zero physical size in the EDID is what breaks
    /// `INPUT_PROP_DIRECT` touch association in the guest.
    pub fn physical_mm(&self) -> (u32, u32) {
        let diagonal_px = self.diagonal_px();
        if diagonal_px <= 0.0 || self.diagonal_mm == 0 {
            return (0, 0);
        }
        let mm_per_px = self.diagonal_mm as f64 / diagonal_px;
        (
            (self.width_px as f64 * mm_per_px) as u32,
            (self.height_px as f64 * mm_per_px) as u32,
        )
    }

    /// The panel's true pixel density in dots per inch — the diagonal in pixels over the diagonal in
    /// inches. 1080×2400 @ 162 mm → ~414 ppi, against a *declared* `density_dpi` of 440.
    ///
    /// Computed from [`physical_mm`](Self::physical_mm) (not from `diagonal_mm` directly) so this is
    /// exactly the density the guest itself derives from the EDID millimetre bytes — the same number
    /// a fingerprinter reads out of `DisplayMetrics.xdpi`/`ydpi`. Returns `0.0` for an unset block.
    pub fn physical_dpi(&self) -> f32 {
        let (w_mm, h_mm) = self.physical_mm();
        let diagonal_mm = ((w_mm as f64).powi(2) + (h_mm as f64).powi(2)).sqrt();
        if diagonal_mm <= 0.0 {
            return 0.0;
        }
        (self.diagonal_px() / (diagonal_mm / 25.4)) as f32
    }

    /// Whether this profile declares a panel at all. `Default` is all-zero — the "no display
    /// declared" state — so every consumer (property emission, bootconfig, `validate`, the VMM's
    /// scanout geometry) branches on this rather than on a sentinel resolution.
    pub fn is_set(&self) -> bool {
        self.width_px != 0
    }

    /// The 128-byte EDID 1.4 base block describing this panel: a CVT reduced-blanking detailed
    /// timing for `width_px × height_px @ refresh_hz` as the preferred mode, plus the derived
    /// physical size. See [`display`] for the bit layout and the invariants it guarantees.
    pub fn edid(&self) -> [u8; 128] {
        let (w_mm, h_mm) = self.physical_mm();
        display::build_edid(self.width_px, self.height_px, self.refresh_hz, w_mm, h_mm)
    }

    /// The panel diagonal in pixels — `hypot(width_px, height_px)`.
    fn diagonal_px(&self) -> f64 {
        ((self.width_px as f64).powi(2) + (self.height_px as f64).powi(2)).sqrt()
    }

    /// Width in density-independent pixels: `width_px / (density_dpi / 160)`. What
    /// `Configuration.screenWidthDp` reports and what every resource qualifier (`sw360dp`, …) keys
    /// off, so an implausible value changes which layouts apps pick — 1080 px @ 440 dpi → 393 dp,
    /// which is what a real `fuxi` reports.
    fn width_dp(&self) -> f32 {
        if self.density_dpi == 0 {
            return 0.0;
        }
        self.width_px as f32 / (self.density_dpi as f32 / 160.0)
    }
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
    pub display: DisplayProfile,
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

        // --- display. `ro.sf.lcd_density` is the density SurfaceFlinger and `DisplayMetrics` read;
        //     the guest image's init sets it from `ro.boot.lcd_density`, which is why
        //     `emit_bootconfig` carries the same value. We NEVER emit `qemu.sf.lcd_density`:
        //     SurfaceFlinger *prefers* that key, and it is on our own detection blocklist
        //     (aemulator-detection PROP_TELLS, from AOSP init.ranchu.rc). A zeroed display block
        //     emits nothing at all — the pre-DisplayProfile behaviour, so nothing half-declares a
        //     panel. The resolution itself is NOT a property (a real device's comes from the DRM
        //     connector); it reaches the guest through the scanout + EDID.
        if self.display.is_set() {
            props.insert(
                "ro.sf.lcd_density".into(),
                self.display.density_dpi.to_string(),
            );
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
        let mut lines = vec![
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
        ];
        // `init` mirrors this into `ro.boot.lcd_density`, and the guest image's init .rc does
        // `setprop ro.sf.lcd_density ${ro.boot.lcd_density}` — so this single line is what actually
        // makes SurfaceFlinger's density match the panel, with no image repack. Emitted only when a
        // panel is declared: `androidboot.lcd_density=0` would be worse than absent.
        if self.display.is_set() {
            lines.push((
                "androidboot.lcd_density".to_string(),
                self.display.density_dpi.to_string(),
            ));
        }
        lines
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

        // 9. Display coherence. Screen metrics are unpermissioned, first-order fingerprint payload
        //    (`DisplayMetrics`), and the panel is described in three places at once — the declared
        //    density, the derived physical size, and the EDID the guest actually parses — so this
        //    section's whole job is to stop those three from disagreeing. A zeroed block declares no
        //    panel and is skipped entirely.
        let d = &self.display;
        if d.is_set() {
            // (a) A partially-declared panel is worse than none: an is_set() block that emits a
            //     zero density or refresh would put `androidboot.lcd_density=0` on the cmdline.
            for (field, value) in [
                ("width_px", d.width_px),
                ("height_px", d.height_px),
                ("density_dpi", d.density_dpi),
                ("refresh_hz", d.refresh_hz),
                ("diagonal_mm", d.diagonal_mm),
            ] {
                if value == 0 {
                    errs.push(CoherenceError::Other(format!(
                        "display.{field} is 0 while the display block is set — all five of \
                         width_px/height_px/density_dpi/refresh_hz/diagonal_mm are required"
                    )));
                }
            }
            // (b) The declared density bucket must be within 25% of the panel's physical dpi. Real
            //     devices round (fuxi: 440 declared against ~414 physical = 1.06x); a desktop-ish
            //     160/96 dpi against a 414 ppi phone panel is the emulator's own giveaway.
            let physical_dpi = d.physical_dpi();
            if physical_dpi > 0.0 && d.density_dpi > 0 {
                let ratio = d.density_dpi as f32 / physical_dpi;
                if !(0.75..=1.25).contains(&ratio) {
                    errs.push(CoherenceError::Other(format!(
                        "display.density_dpi {} is {ratio:.2}x the panel's physical {physical_dpi:.0} \
                         dpi (must be within 0.75..1.25 — a real device rounds, it does not invent)",
                        d.density_dpi
                    )));
                }
            }
            // (c) Aspect ratio, in the panel's native portrait orientation. A phone is 1.7..2.4:1;
            //     0.75 (a transposed 4:3 landscape panel) is precisely the incoherence this whole
            //     block exists to prevent.
            if d.width_px > 0 {
                let aspect = d.height_px as f32 / d.width_px as f32;
                if !(1.7..=2.4).contains(&aspect) {
                    errs.push(CoherenceError::Other(format!(
                        "display aspect ratio {}x{} = {aspect:.2}:1 is not a phone panel in its \
                         native portrait orientation (expected 1.7..2.4)",
                        d.width_px, d.height_px
                    )));
                }
            }
            // (d) Width in dp — `Configuration.screenWidthDp`, which selects every resource
            //     qualifier. Outside 320..600 dp the device is a watch or a tablet, not this phone.
            let width_dp = d.width_dp();
            if d.density_dpi > 0 && !(320.0..=600.0).contains(&width_dp) {
                errs.push(CoherenceError::Other(format!(
                    "display width is {width_dp:.0} dp ({} px @ {} dpi) — outside the 320..600 dp \
                     a phone reports; every resource qualifier keys off this",
                    d.width_px, d.density_dpi
                )));
            }
            // (e) The EDID must be well-formed and must round-trip the declared mode. The EDID's
            //     preferred timing and the scanout are a hard coherence pair (the guest kernel
            //     rejects a preferred mode more than 16 px off the scanout, and reports MODE_BAD
            //     without any host-visible error — a silently blank screen).
            let edid = d.edid();
            if !display::checksum_is_valid(&edid) {
                errs.push(CoherenceError::Other(
                    "the emitted EDID base block does not sum to 0 mod 256 — a guest EDID parser \
                     rejects it outright"
                        .into(),
                ));
            }
            match display::decode_dtd(&edid) {
                None => errs.push(CoherenceError::Other(
                    "the emitted EDID declares no detailed timing (descriptor 1 has a zero pixel \
                     clock), so the guest has no preferred mode"
                        .into(),
                )),
                Some(t) => {
                    if t.width_px != d.width_px || t.height_px != d.height_px {
                        errs.push(CoherenceError::Other(format!(
                            "the emitted EDID's preferred mode is {}x{} but the profile declares \
                             {}x{} (a width that is not a multiple of 8 cannot round-trip a CVT \
                             detailed timing)",
                            t.width_px, t.height_px, d.width_px, d.height_px
                        )));
                    }
                    if (t.refresh_hz - d.refresh_hz as f32).abs() > 1.0 {
                        errs.push(CoherenceError::Other(format!(
                            "the emitted EDID's preferred mode runs at {:.2} Hz but the profile \
                             declares {} Hz",
                            t.refresh_hz, d.refresh_hz
                        )));
                    }
                    if t.width_mm == 0 || t.height_mm == 0 {
                        errs.push(CoherenceError::Other(
                            "the emitted EDID carries a zero physical image size — the guest's \
                             DisplayViewport, and with it INPUT_PROP_DIRECT touch association, \
                             depends on it being non-zero"
                                .into(),
                        ));
                    }
                }
            }
        }

        // 10. GPU coherence — the cross-checks `GpuProfile`'s doc comment has always promised.
        //     Two of these guard *rendering*, not just plausibility: an EGL name the image cannot
        //     load means no GL at all, and a claimed capability the host lacks means
        //     `vkCreateDevice` fails. Those two are the reason this section exists in the identity
        //     crate (whose tests run in CI) rather than in the workspace-excluded VMM crate.
        let g = &self.gpu;

        // (a) `ro.hardware.egl` must name an EGL implementation the guest image ships, because
        //     Android's loader dlopen()s `libEGL_${ro.hardware.egl}.so` with no fallback. Checked
        //     whenever the value is non-empty — a profile can declare no GPU and still emit this
        //     property.
        if !g.ro_hardware_egl.is_empty()
            && !EGL_IMPLEMENTATIONS_SHIPPED.contains(&g.ro_hardware_egl.as_str())
        {
            errs.push(CoherenceError::GpuIncoherent(format!(
                "ro.hardware.egl `{}` names no EGL implementation the guest image ships: Android \
                 dlopen()s `libEGL_{}.so`, there is no such library in the image, and the loader \
                 has no fallback — GL disappears entirely. Legal values: {}",
                g.ro_hardware_egl,
                g.ro_hardware_egl,
                EGL_IMPLEMENTATIONS_SHIPPED.join("|")
            )));
        }

        // (b) `deviceType` may never be Cpu. The struct doc has said so since it was written;
        //     nothing enforced it. VK_PHYSICAL_DEVICE_TYPE_CPU is on our own detection blocklist
        //     (SwiftShader/lavapipe), and no phone reports it.
        if g.vk_device_type == VkDeviceType::Cpu {
            errs.push(CoherenceError::GpuIncoherent(
                "vk_device_type is Cpu — VK_PHYSICAL_DEVICE_TYPE_CPU is a SwiftShader/lavapipe \
                 tell on our own blocklist and no physical phone reports it"
                    .into(),
            ));
        }

        // (c) The claimed GLES level may not exceed what the boot cmdline encodes.
        if !g.gl_version.is_empty() {
            let (cap_major, cap_minor) = decode_gles_version(BOOT_OPENGLES_VERSION);
            match parse_gl_es_version(&g.gl_version) {
                None => errs.push(CoherenceError::GpuIncoherent(format!(
                    "gl_version `{}` does not parse as an `OpenGL ES <major>.<minor> …` string, so \
                     it cannot be checked against androidboot.opengles.version",
                    g.gl_version
                ))),
                Some((major, minor)) if (major, minor) > (cap_major, cap_minor) => {
                    errs.push(CoherenceError::GpuIncoherent(format!(
                        "gl_version claims GLES {major}.{minor} but androidboot.opengles.version is \
                         {BOOT_OPENGLES_VERSION} = GLES {cap_major}.{cap_minor}. Raising the \
                         cmdline is NOT the fix: the extra level is unbacked by the host, so an app \
                         that reads the higher GL_VERSION and then uses the stage it implies gets \
                         nothing — a worse tell than the lower version AND broken rendering"
                    )));
                }
                Some(_) => {}
            }
        }

        // (d) vendorID ↔ GL_VENDOR ↔ driverID must be one of the triples a real device reports
        //     together, and the renderer must belong to that vendor's part family.
        if g.is_set() {
            match GPU_VENDOR_TRIPLES
                .iter()
                .find(|t| t.vk_vendor_id == g.vk_vendor_id)
            {
                None => errs.push(CoherenceError::GpuIncoherent(format!(
                    "vk_vendor_id 0x{:04x} is not a mobile GPU vendor this crate can keep coherent \
                     — add it to GPU_VENDOR_TRIPLES with its GL_VENDOR and driverID, or the three \
                     fields cannot be cross-checked at all. Known: {}",
                    g.vk_vendor_id,
                    GPU_VENDOR_TRIPLES
                        .iter()
                        .map(|t| format!("0x{:04x}/{}", t.vk_vendor_id, t.gl_vendor))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
                Some(triple) => {
                    if !contains_ignore_ascii_case(&g.gl_vendor, triple.gl_vendor) {
                        errs.push(CoherenceError::GpuIncoherent(format!(
                            "vk_vendor_id 0x{:04x} is {} but gl_vendor is `{}` (must contain `{}`)",
                            g.vk_vendor_id, triple.gl_vendor, g.gl_vendor, triple.gl_vendor
                        )));
                    }
                    if g.vk_driver_id != triple.vk_driver_id {
                        errs.push(CoherenceError::GpuIncoherent(format!(
                            "vk_vendor_id 0x{:04x} ({}) requires vk_driver_id `{}`, got `{}`",
                            g.vk_vendor_id, triple.gl_vendor, triple.vk_driver_id, g.vk_driver_id
                        )));
                    }
                    if !contains_ignore_ascii_case(&g.gl_renderer, triple.part_family) {
                        errs.push(CoherenceError::GpuIncoherent(format!(
                            "vk_vendor_id 0x{:04x} ({}) ships `{}` parts but gl_renderer is `{}`",
                            g.vk_vendor_id, triple.gl_vendor, triple.part_family, g.gl_renderer
                        )));
                    }
                }
            }

            // (e) `vk_device_name` must name the same part as `gl_renderer` — an app reading
            //     GL_RENDERER and one reading VkPhysicalDeviceProperties::deviceName must not learn
            //     two different GPUs.
            match (
                gpu_part_token(&g.gl_renderer),
                gpu_part_token(&g.vk_device_name),
            ) {
                (Some(gl_part), Some(vk_part)) if gl_part != vk_part => {
                    errs.push(CoherenceError::GpuIncoherent(format!(
                        "gl_renderer `{}` names part `{gl_part}` but vk_device_name `{}` names \
                         `{vk_part}`",
                        g.gl_renderer, g.vk_device_name
                    )));
                }
                (Some(gl_part), None) => errs.push(CoherenceError::GpuIncoherent(format!(
                    "gl_renderer `{}` names part `{gl_part}` but vk_device_name `{}` names no part \
                     at all",
                    g.gl_renderer, g.vk_device_name
                ))),
                _ => {}
            }

            // (f) No claim may imply a capability the host provably lacks. Over-claiming here is
            //     not cosmetic: the guest requests the feature, `vkCreateDevice` returns
            //     VK_ERROR_FEATURE_NOT_PRESENT, and there is no graphics at all.
            let claims = [
                g.gl_vendor.as_str(),
                g.gl_renderer.as_str(),
                g.gl_version.as_str(),
                g.vk_device_name.as_str(),
                g.vk_driver_id.as_str(),
            ]
            .join(" ")
            .to_ascii_lowercase();
            let gles = parse_gl_es_version(&g.gl_version);
            for entry in HOST_UNBACKABLE {
                let via_gles = match (entry.implied_by_gles_3_minor, gles) {
                    (Some(min_minor), Some((major, minor))) => (major, minor) >= (3, min_minor),
                    _ => false,
                };
                let via_needle = entry
                    .claim_needles
                    .iter()
                    .find(|needle| claims.contains(**needle));
                if via_gles || via_needle.is_some() {
                    let why = if via_gles {
                        format!(
                            "a GLES {}.{} claim",
                            gles.map(|(major, _)| major).unwrap_or(0),
                            gles.map(|(_, minor)| minor).unwrap_or(0)
                        )
                    } else {
                        format!("the claim string `{}`", via_needle.copied().unwrap_or(""))
                    };
                    errs.push(CoherenceError::GpuIncoherent(format!(
                        "{why} implies `{}`, which the host cannot back: {}. The guest would \
                         request it and vkCreateDevice would return VK_ERROR_FEATURE_NOT_PRESENT — \
                         no graphics at all",
                        entry.capability, entry.host_evidence
                    )));
                }
            }
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

/// Case-insensitive substring test. Both sides are ASCII in every identity field we compare.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// Decode an `androidboot.opengles.version` integer (`major << 16 | minor`) into `(major, minor)` —
/// `196609` → `(3, 1)`.
fn decode_gles_version(encoded: u32) -> (u32, u32) {
    (encoded >> 16, encoded & 0xFFFF)
}

/// Parse `(major, minor)` out of a `glGetString(GL_VERSION)` string. GLES spells it
/// `"OpenGL ES <major>.<minor>[.<release>] <vendor-specific>"`, e.g.
/// `"OpenGL ES 3.1 V@0744.0 (GIT@…)"` → `(3, 1)`. Returns `None` if the mandated prefix or the
/// version number is missing — which is itself a coherence failure, not a reason to skip the check.
fn parse_gl_es_version(gl_version: &str) -> Option<(u32, u32)> {
    const PREFIX: &str = "opengl es";
    let lower = gl_version.to_ascii_lowercase();
    let rest = &lower[lower.find(PREFIX)? + PREFIX.len()..];
    let token = rest.split_whitespace().next()?;
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    // A missing minor is 0 ("OpenGL ES 3"); a trailing release number is ignored.
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

/// The model token of a GPU marketing name: the **first** whitespace/punctuation-separated token
/// containing a digit, uppercased. `"Adreno (TM) 740"` → `"740"`, `"Mali-G715 MC11"` → `"G715"`,
/// `"PowerVR Rogue GE8320"` → `"GE8320"`.
///
/// First rather than last on purpose: trailing tokens are configuration suffixes (core counts,
/// driver build ids) that the Vulkan `deviceName` and the GL renderer string do not have to share,
/// whereas the model number is the part itself.
fn gpu_part_token(name: &str) -> Option<String> {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .find(|token| token.chars().any(|c| c.is_ascii_digit()))
        .map(|token| token.to_ascii_uppercase())
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
        // `angle`, NOT `adreno`: Android dlopen()s `libEGL_${ro.hardware.egl}.so` and the image
        // ships no `libEGL_adreno.so`, so the vendor name would mean no GL at all (Phase 4.1).
        assert_eq!(
            props.get("ro.hardware.egl").map(String::as_str),
            Some("angle")
        );
        assert!(EGL_IMPLEMENTATIONS_SHIPPED.contains(&"angle"));
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

    // --- display -------------------------------------------------------------------------------

    #[test]
    fn display_profile_is_coherent() {
        let p = profiles::reference_profile();
        // The reference panel passes every arm of validate's display section.
        assert_eq!(p.validate(), Ok(()));
        let d = p.display;
        assert!(d.is_set());
        assert_eq!((d.width_px, d.height_px), (1080, 2400));
        assert_eq!((d.density_dpi, d.refresh_hz, d.diagonal_mm), (440, 60, 162));
        // 162 mm diagonal over a 1080x2400 pixel diagonal -> 66x147 mm of active area.
        assert_eq!(d.physical_mm(), (66, 147));
        // ~414 ppi physical against the declared 440 bucket: a ratio of 1.06.
        let physical = d.physical_dpi();
        assert!(
            (413.0..=416.0).contains(&physical),
            "physical dpi should be ~414, got {physical}"
        );
        assert!((d.density_dpi as f32 / physical - 1.06).abs() < 0.02);
        // 393 dp of width — what a real fuxi reports, and inside validate's 320..600 dp band.
        assert!(
            (392.0..=394.0).contains(&d.width_dp()),
            "width should be ~393 dp, got {}",
            d.width_dp()
        );

        // A desktop 96 dpi against this panel is incoherent, and validate must name the field.
        let mut bad = p.clone();
        bad.display.density_dpi = 96;
        let errs = bad
            .validate()
            .expect_err("96 dpi on a 414 ppi phone panel must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::Other(m) if m.contains("display.density_dpi")),
            "Other(display.density_dpi)",
        );

        // A transposed (landscape 4:3) panel is the exact incoherence this section prevents.
        let mut landscape = p.clone();
        landscape.display = DisplayProfile {
            width_px: 1024,
            height_px: 768,
            density_dpi: 320,
            refresh_hz: 60,
            diagonal_mm: 162,
        };
        let errs = landscape
            .validate()
            .expect_err("a 4:3 landscape panel must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::Other(m) if m.contains("aspect ratio")),
            "Other(aspect ratio)",
        );

        // A width that is not a multiple of 8 cannot round-trip a CVT detailed timing, so the EDID
        // would advertise a preferred mode that disagrees with the scanout.
        let mut off_grid = p.clone();
        off_grid.display.width_px = 1084;
        let errs = off_grid
            .validate()
            .expect_err("an off-cell-granularity width must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::Other(m) if m.contains("preferred mode")),
            "Other(preferred mode)",
        );
    }

    #[test]
    fn edid_round_trips_the_declared_mode() {
        let d = profiles::reference_profile().display;
        let edid = d.edid();

        // Structurally valid: header magic + a checksum that zeroes the block.
        assert_eq!(edid[0..8], [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        assert_eq!(edid.iter().map(|b| *b as u32).sum::<u32>() % 256, 0);
        assert!(display::checksum_is_valid(&edid));
        // Bytes 21/22 (cm) — non-zero, or the guest has no physical size and INPUT_PROP_DIRECT
        // touch association fails.
        assert!(edid[21] > 0 && edid[22] > 0, "cm image size must be > 0");
        // Established timings: byte 36 bit 3 is "this panel does 1024x768@60". It must be clear —
        // 1024x768 is not this panel's native mode.
        assert_eq!(&edid[35..38], &[0x00, 0x00, 0x00]);

        // DECODE descriptor 1 back out: the declared mode, not a frozen VESA DMT block.
        let t = display::decode_dtd(&edid).expect("DTD-1 must be a real detailed timing");
        assert_eq!(t.width_px, 1080);
        assert_eq!(t.height_px, 2400);
        assert!(
            (t.refresh_hz - 60.0).abs() <= 1.0,
            "decoded refresh {} Hz, declared 60",
            t.refresh_hz
        );
        assert_eq!((t.width_mm, t.height_mm), (66, 147));
        assert_eq!(t.width_mm, d.physical_mm().0);
        assert_eq!(t.height_mm, d.physical_mm().1);
        // The CVT reduced-blanking derivation for 1080x2400@60.
        assert_eq!((t.h_total, t.v_total), (1240, 2469));
        assert_eq!(t.pixel_clock_hz, 183_690_000);
        assert_eq!((t.h_front_porch, t.h_sync_width), (48, 32));
        assert_eq!((t.v_front_porch, t.v_sync_width), (3, 4));
    }

    #[test]
    fn bootconfig_carries_lcd_density() {
        let bc = profiles::reference_profile().emit_bootconfig();
        let line = bc
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            line.contains("androidboot.lcd_density=440"),
            "bootconfig must carry the panel density: {line}"
        );
        // `qemu.sf.lcd_density` is on our own detection blocklist and SurfaceFlinger prefers it.
        for (key, _) in &bc {
            assert!(
                !key.contains("qemu"),
                "bootconfig must never carry a qemu key: {key}"
            );
        }
        // ...and the property side carries the same value, from the same field.
        let props = profiles::reference_profile().emit_properties();
        assert_eq!(
            props.get("ro.sf.lcd_density").map(String::as_str),
            Some("440")
        );
        assert!(!props.contains_key("qemu.sf.lcd_density"));
    }

    #[test]
    fn a_default_profile_emits_no_display_properties() {
        let p = DeviceProfile::default();
        assert!(!p.display.is_set(), "Default must stay all-zero");
        assert_eq!(p.display.physical_mm(), (0, 0));
        assert_eq!(p.display.physical_dpi(), 0.0);
        assert!(!p.emit_properties().contains_key("ro.sf.lcd_density"));
        assert!(!p
            .emit_bootconfig()
            .iter()
            .any(|(k, _)| k == "androidboot.lcd_density"));
        // ...and validate's display section is skipped entirely rather than firing five times.
        let display_errs = p
            .validate()
            .expect_err("a default profile is incoherent for other reasons")
            .into_iter()
            .filter(|e| matches!(e, CoherenceError::Other(m) if m.contains("display")))
            .count();
        assert_eq!(display_errs, 0, "an unset display block must not validate");
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
        // ...and the GPU block must match field-for-field, which the property map cannot check:
        // only `ro.hardware.egl` of it is ever emitted as a property, so `gl_version`,
        // `vk_device_id` and `vk_subgroup_size` could otherwise drift between the twins unnoticed.
        assert_eq!(loaded.gpu, profiles::reference_profile().gpu);
        assert_eq!(
            loaded.gpu.gl_version, "OpenGL ES 3.1 V@0744.0 (GIT@a0d5f2b, Ie4a2f9d70e, 1675814076)",
            "the twin must claim GLES 3.1 — 3.2 needs a geometry stage this host does not have"
        );
        assert_eq!(loaded.gpu.ro_hardware_egl, "angle");
        // The same spoof, from either twin.
        assert_eq!(
            loaded.gpu.host_spoof(0x51a7_c0de),
            profiles::reference_profile().gpu.host_spoof(0x51a7_c0de)
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

    // --- GPU: the host-side identity policy (Phase 4.1-4.5) ------------------------------------

    /// The measured host's own `pipelineCacheUUID`, verbatim from
    /// `crates/aemulator-hvf/gfxstream/evidence/host-vulkan-identity.json`
    /// (`properties.pipelineCacheUUID`, and bytes 16..32 of `pipelineCacheHeader.first32BytesHex`).
    /// It is **stable across reboots** on that host, which is exactly what makes it a permanent,
    /// unpermissioned, cross-app device identifier — and why nothing we mint may reproduce it.
    const HOST_PIPELINE_CACHE_UUID: [u8; 16] = [
        0x16, 0x55, 0xd6, 0xf0, 0x9e, 0xe4, 0xd2, 0xa9, 0x99, 0x72, 0xc5, 0x5d, 0xa3, 0x0e, 0x2a,
        0x0c,
    ];

    /// Spoofs whose structural invariants must hold unconditionally: the default (passthrough), the
    /// bundled reference, and an *adversarial* profile that claims everything the host lacks. The
    /// adversarial case would be rejected by `validate`, and that is the point — even a profile that
    /// should never ship must not be able to produce a set-bit.
    fn spoof_matrix() -> Vec<(&'static str, GpuSpoof)> {
        let mut adversarial = profiles::reference_profile();
        adversarial.gpu.gl_version = "OpenGL ES 3.2 V@0744.0 ray_query".into();
        adversarial.gpu.vk_device_id = 0x4305_0a01;
        adversarial.gpu.vk_subgroup_size = 128;
        vec![
            ("default", GpuProfile::default().host_spoof(0)),
            (
                "reference",
                profiles::reference_profile().gpu.host_spoof(0x51a7_c0de),
            ),
            ("adversarial", adversarial.gpu.host_spoof(1)),
        ]
    }

    #[test]
    fn host_spoof_only_ever_clears_features_never_sets_them() {
        // THE invariant that prevents total loss of graphics: reporting a feature TRUE that the host
        // reports FALSE makes the guest request it, `vkCreateDevice` returns
        // VK_ERROR_FEATURE_NOT_PRESENT, and there is no graphics at all.
        for (name, spoof) in spoof_matrix() {
            for (feature, reported) in spoof.feature_overrides() {
                assert!(
                    !reported,
                    "{name}: GpuSpoof reported `{feature}` TRUE — a set-bit is unrepresentable by \
                     construction, so this means a new field broke the invariant"
                );
            }
            // The two features that are FALSE on the measured host must never be touched at all:
            // there is no safe direction to move them, only a clamp that is already redundant.
            let touched: Vec<&str> = spoof
                .feature_overrides()
                .iter()
                .map(|(feature, _)| *feature)
                .collect();
            for forbidden in ["geometryShader", "wideLines"] {
                assert!(
                    !touched.contains(&forbidden),
                    "{name}: `{forbidden}` is FALSE on this host and must never appear in a spoof"
                );
            }
        }

        // Structural half: `feature_overrides` can only report what someone remembered to add to
        // it, so also pin the STRUCT — every boolean in `GpuSpoof` must be a `clear_*` flag (or the
        // one non-feature `strip_extensions`). This is what actually catches a future
        // `set_geometry_shader: bool`, which would be total loss of graphics rather than a tell.
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"),
        )
        .expect("this crate's own source must be readable");
        let decl = source
            .split_once("pub struct GpuSpoof {")
            .expect("GpuSpoof must be declared in this file")
            .1
            .split_once("\n}")
            .expect("GpuSpoof's declaration must be terminated")
            .0;
        let bools: Vec<&str> = decl
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub "))
            .filter_map(|field| field.split_once(": bool,"))
            .map(|(name, _)| name)
            .collect();
        assert!(
            bools.len() >= 3,
            "expected to find GpuSpoof's boolean fields, found {bools:?} — the scan broke, and a \
             vacuous pass here is worse than no test"
        );
        for field in bools {
            assert!(
                field.starts_with("clear_") || field == "strip_extensions",
                "GpuSpoof field `{field}` is a boolean that is neither a `clear_*` feature flag nor \
                 `strip_extensions`. If it reports a feature TRUE, the guest requests it, \
                 vkCreateDevice returns VK_ERROR_FEATURE_NOT_PRESENT, and there is NO graphics."
            );
        }
    }

    #[test]
    fn multi_viewport_false_implies_max_viewports_one() {
        // The Vulkan spec REQUIRES maxViewports == 1 when multiViewport is unsupported. Clearing one
        // without the other yields an internally invalid device — a louder tell than the host's 16.
        for (name, spoof) in spoof_matrix() {
            if spoof.clear_multi_viewport {
                assert_eq!(
                    spoof.max_viewports, 1,
                    "{name}: multiViewport cleared without maxViewports = 1"
                );
            } else {
                assert_eq!(
                    spoof.max_viewports, 0,
                    "{name}: maxViewports touched without clearing multiViewport"
                );
            }
        }
        // And the reference really does exercise the pairing (not vacuously skipped).
        let spoof = profiles::reference_profile().gpu.host_spoof(1);
        assert!(spoof.clear_multi_viewport);
        assert_eq!(spoof.max_viewports, 1);
    }

    #[test]
    fn a_default_profile_yields_an_all_sentinel_spoof() {
        // Exact passthrough when unconfigured — the property that makes an applier whose setter is
        // never called agree with an applier handed a default profile.
        let spoof = GpuProfile::default().host_spoof(0xdead_beef);
        assert!(spoof.is_passthrough(), "default spoof must be passthrough");
        assert_eq!(spoof.vendor_id, 0);
        assert_eq!(spoof.device_id, 0);
        assert_eq!(spoof.device_name, "");
        assert_eq!(spoof.pipeline_cache_uuid, [0u8; 16]);
        assert!(!spoof.clear_texture_compression_bc);
        assert!(!spoof.clear_multi_viewport);
        assert_eq!(spoof.max_viewports, 0);
        assert_eq!(spoof.subgroup_size, 0);
        assert!(!spoof.strip_extensions);
        assert!(spoof.feature_overrides().is_empty());
        assert!(!DeviceProfile::default().gpu.is_set());
        // ...and the configured reference is NOT passthrough, or the test above proves nothing.
        let reference = profiles::reference_profile().gpu.host_spoof(7);
        assert!(!reference.is_passthrough());
        assert_eq!(reference.vendor_id, 0x5143);
        assert_eq!(reference.device_name, "Adreno (TM) 740");
        assert!(reference.strip_extensions);
        // The reference leaves deviceID and subgroupSize alone rather than fabricating them.
        assert_eq!(reference.device_id, 0);
        assert_eq!(reference.subgroup_size, 0);
    }

    #[test]
    fn instance_uuid_is_v4_shaped_and_deterministic() {
        let uuid = instance_uuid(0x51a7_c0de, UUID_KIND_PIPELINE_CACHE, 0);
        // RFC-4122 v4 shape: version nibble 4, variant bits 0b10.
        assert_eq!(
            uuid[6] & 0xF0,
            0x40,
            "version nibble must be 4: {uuid:02x?}"
        );
        assert_eq!(
            uuid[8] & 0xC0,
            0x80,
            "variant bits must be 0b10: {uuid:02x?}"
        );
        assert_ne!(
            uuid, [0u8; 16],
            "an all-zero UUID is the passthrough sentinel"
        );
        // Deterministic for the same (seed, kind, index) — a pipeline cache must survive a relaunch.
        assert_eq!(
            uuid,
            instance_uuid(0x51a7_c0de, UUID_KIND_PIPELINE_CACHE, 0)
        );
        // ...and distinct across every input. Seed: two instances must not be linkable. Kind: the
        // pipeline-cache UUID and a resource UUID minted from one seed must not collide. Index:
        // per-resource UUIDs must differ.
        assert_ne!(
            uuid,
            instance_uuid(0x51a7_c0df, UUID_KIND_PIPELINE_CACHE, 0)
        );
        assert_ne!(uuid, instance_uuid(0x51a7_c0de, UUID_KIND_RESOURCE, 0));
        assert_ne!(
            uuid,
            instance_uuid(0x51a7_c0de, UUID_KIND_PIPELINE_CACHE, 1)
        );
        // Every kind/index combination stays v4-shaped and non-zero.
        for kind in [
            UUID_KIND_PIPELINE_CACHE,
            UUID_KIND_RESOURCE,
            UUID_KIND_DEVICE,
            UUID_KIND_DRIVER,
        ] {
            for index in 0..8 {
                let u = instance_uuid(0, kind, index);
                assert_eq!(u[6] & 0xF0, 0x40);
                assert_eq!(u[8] & 0xC0, 0x80);
                assert_ne!(u, [0u8; 16]);
            }
        }
        // No eight-zero-byte run in the middle, which is what the `b"AEMU"` magic it replaces has.
        assert_ne!(&uuid[4..12], &[0u8; 8]);
    }

    #[test]
    fn pipeline_cache_uuid_is_not_the_hosts() {
        // Killing this identifier is the single highest-value item in the whole GPU phase: the
        // host's is stable across reboots, so it is a permanent hardware ID readable by any app
        // with no permission, surviving a factory reset.
        let profile = profiles::reference_profile();
        let mut seen = std::collections::BTreeSet::new();
        for seed in [0u64, 1, 0x51a7_c0de, u64::MAX] {
            let spoof = profile.gpu.host_spoof(seed);
            assert_ne!(
                spoof.pipeline_cache_uuid, HOST_PIPELINE_CACHE_UUID,
                "spoofed pipelineCacheUUID must never be the host's 1655d6f0… (seed {seed})"
            );
            assert_ne!(spoof.pipeline_cache_uuid, [0u8; 16]);
            // Stable for a given instance seed (so a shader cache still hits)...
            assert_eq!(
                spoof.pipeline_cache_uuid,
                profile.gpu.host_spoof(seed).pipeline_cache_uuid
            );
            seen.insert(spoof.pipeline_cache_uuid);
        }
        // ...and different per instance, so two instances are not linkable by it.
        assert_eq!(
            seen.len(),
            4,
            "each instance seed must mint a distinct UUID"
        );
        // The baseline constant really is the host's, so the assertion above is not vacuous.
        assert_eq!(
            HOST_PIPELINE_CACHE_UUID
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "1655d6f09ee4d2a99972c55da30e2a0c"
        );
    }

    #[test]
    fn validate_rejects_an_egl_the_image_cannot_load() {
        // The bug this rule exists for: `adreno` names `libEGL_adreno.so`, which the guest image
        // does not ship, and Android's loader has no fallback — GL disappears entirely.
        let mut p = profiles::reference_profile();
        p.gpu.ro_hardware_egl = "adreno".into();
        let errs = p
            .validate()
            .expect_err("an unloadable ro.hardware.egl must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("libEGL_adreno.so")),
            "GpuIncoherent(libEGL_adreno.so)",
        );
        // Every value the image DOES ship passes.
        for shipped in EGL_IMPLEMENTATIONS_SHIPPED {
            let mut ok = profiles::reference_profile();
            ok.gpu.ro_hardware_egl = (*shipped).to_string();
            assert_eq!(ok.validate(), Ok(()), "`{shipped}` must be accepted");
        }
    }

    #[test]
    fn validate_rejects_a_gl_version_above_the_cmdline_cap() {
        // GLES 3.2 is what a real Adreno 740 reports and what this profile used to claim. The boot
        // cmdline encodes 3.1, and raising the cmdline is not the fix — the host has no geometry
        // stage to back the extra level.
        assert_eq!(decode_gles_version(BOOT_OPENGLES_VERSION), (3, 1));
        let mut p = profiles::reference_profile();
        p.gpu.gl_version = "OpenGL ES 3.2 V@0744.0 (GIT@a0d5f2b, Ie4a2f9d70e, 1675814076)".into();
        let errs = p
            .validate()
            .expect_err("GLES 3.2 over a 196609 cmdline must fail validate");
        assert_has(
            &errs,
            |e| {
                matches!(e, CoherenceError::GpuIncoherent(m)
                    if m.contains("androidboot.opengles.version"))
            },
            "GpuIncoherent(opengles.version cap)",
        );
        // A version string that is not a GLES version at all is also a failure, not a skip.
        let mut garbled = profiles::reference_profile();
        garbled.gpu.gl_version = "Adreno driver 0744".into();
        let errs = garbled.validate().expect_err("unparseable gl_version");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("does not parse")),
            "GpuIncoherent(unparseable gl_version)",
        );
    }

    #[test]
    fn validate_rejects_a_vendor_triple_mismatch() {
        // gl_vendor disagreeing with vendorID.
        let mut gl = profiles::reference_profile();
        gl.gpu.gl_vendor = "ARM".into();
        assert_has(
            &gl.validate()
                .expect_err("Qualcomm vendorID + ARM GL_VENDOR"),
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("gl_vendor")),
            "GpuIncoherent(gl_vendor)",
        );
        // driverID disagreeing with vendorID.
        let mut driver = profiles::reference_profile();
        driver.gpu.vk_driver_id = "VK_DRIVER_ID_MESA_TURNIP".into();
        assert_has(
            &driver.validate().expect_err("Qualcomm vendorID + turnip"),
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("vk_driver_id")),
            "GpuIncoherent(vk_driver_id)",
        );
        // A vendorID outside the table is rejected fail-closed: nothing could cross-check it.
        let mut unknown = profiles::reference_profile();
        unknown.gpu.vk_vendor_id = 0x8086; // Intel — not a mobile GPU vendor
        assert_has(
            &unknown.validate().expect_err("an unknown vendorID"),
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("GPU_VENDOR_TRIPLES")),
            "GpuIncoherent(unknown vendorID)",
        );
        // The renderer must belong to the claimed vendor's part family.
        let mut family = profiles::reference_profile();
        family.gpu.gl_renderer = "Mali-G715".into();
        family.gpu.vk_device_name = "Mali-G715".into();
        assert_has(
            &family
                .validate()
                .expect_err("Qualcomm vendorID + a Mali part"),
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("Adreno")),
            "GpuIncoherent(part family)",
        );
    }

    #[test]
    fn validate_rejects_a_renderer_that_disagrees_with_the_vulkan_device_name() {
        let mut p = profiles::reference_profile();
        p.gpu.vk_device_name = "Adreno (TM) 730".into();
        let errs = p
            .validate()
            .expect_err("GL_RENDERER 740 + deviceName 730 must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("names part `740`")),
            "GpuIncoherent(part disagreement)",
        );
    }

    #[test]
    fn validate_rejects_a_cpu_device_type() {
        let mut p = profiles::reference_profile();
        p.gpu.vk_device_type = VkDeviceType::Cpu;
        let errs = p
            .validate()
            .expect_err("VK_PHYSICAL_DEVICE_TYPE_CPU must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("vk_device_type is Cpu")),
            "GpuIncoherent(Cpu deviceType)",
        );
    }

    #[test]
    fn validate_rejects_a_profile_claiming_host_unbackable_capability() {
        // (a) via a claim needle, at a GLES level that is otherwise legal — so this rule is proven
        //     independent of the version cap.
        let mut ray = profiles::reference_profile();
        ray.gpu.gl_version = "OpenGL ES 3.1 V@0744.0 (GIT@a0d5f2b, ray_query)".into();
        let errs = ray
            .validate()
            .expect_err("a ray-query claim must fail validate");
        assert_has(
            &errs,
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("VK_KHR_ray_query")),
            "GpuIncoherent(VK_KHR_ray_query)",
        );
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, CoherenceError::GpuIncoherent(m)
                if m.contains("androidboot.opengles.version"))),
            "the unbackable rule must fire on its own, not as a side effect of the version cap"
        );
        // (b) via the GLES level: 3.2 implies a geometry stage the host does not have. This is the
        //     rule that would have caught the profile this crate shipped before Phase 4.2.
        let mut gles32 = profiles::reference_profile();
        gles32.gpu.gl_version = "OpenGL ES 3.2 V@0744.0".into();
        assert_has(
            &gles32.validate().expect_err("GLES 3.2 must fail validate"),
            |e| matches!(e, CoherenceError::GpuIncoherent(m) if m.contains("geometryShader")),
            "GpuIncoherent(geometryShader)",
        );
    }

    #[test]
    fn gl_version_and_part_token_helpers_are_correct() {
        assert_eq!(
            parse_gl_es_version("OpenGL ES 3.1 V@0744.0 (GIT@a0d5f2b)"),
            Some((3, 1))
        );
        assert_eq!(parse_gl_es_version("OpenGL ES 3.2"), Some((3, 2)));
        assert_eq!(parse_gl_es_version("OpenGL ES 3"), Some((3, 0)));
        assert_eq!(parse_gl_es_version("OpenGL ES 3.1.0"), Some((3, 1)));
        assert_eq!(parse_gl_es_version("OpenGL 4.6 (Core Profile)"), None);
        assert_eq!(decode_gles_version(196_609), (3, 1));
        assert_eq!(decode_gles_version(196_610), (3, 2));

        assert_eq!(gpu_part_token("Adreno (TM) 740").as_deref(), Some("740"));
        assert_eq!(gpu_part_token("Mali-G715 MC11").as_deref(), Some("G715"));
        assert_eq!(
            gpu_part_token("PowerVR Rogue GE8320").as_deref(),
            Some("GE8320")
        );
        assert_eq!(gpu_part_token("Adreno").as_deref(), None);
        assert!(contains_ignore_ascii_case("Adreno (TM) 740", "adreno"));
        assert!(!contains_ignore_ascii_case("Adreno (TM) 740", "mali"));
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
