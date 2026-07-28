//! Bundled reference profiles.
//!
//! [`reference_profile`] returns a **minimal but coherent** literal for a Xiaomi 13 (`fuxi`) /
//! Snapdragon 8 Gen 2 (`SM8550`). It is a stub: the identity implementer (task #2) expands this
//! into the full bundled catalogue with the complete 5×5 property matrix, per-partition
//! fingerprints, and full sensor/socinfo detail. The values here are self-consistent so downstream
//! crates and tests have a valid fixture; see `profiles/xiaomi_13.toml` (the on-disk twin) for a
//! field-by-field provenance note on which values are verified platform data and which are
//! shape-accurate derivations.

use crate::{
    AbiProfile, BuildIdentity, DeviceProfile, DisplayProfile, GpuProfile, MidrCluster,
    NetworkProfile, ProfileMeta, SensorSpec, SocProfile, Socinfo, TelephonyProfile, VerifiedBoot,
    VkDeviceType,
};

/// A minimal, coherent reference `DeviceProfile` (Xiaomi 13 `fuxi` / SM8550).
///
/// Fingerprint this yields: `Xiaomi/fuxi/fuxi:13/TKQ1.220829.002/V14.0.6.0.TMCCNXM:user/release-keys`.
pub fn reference_profile() -> DeviceProfile {
    DeviceProfile {
        meta: ProfileMeta {
            name: "Xiaomi 13 (fuxi) / SM8550".to_string(),
            source: "Snapdragon 8 Gen 2 (SM8550/kalama) public platform data + a Xiaomi 13 `fuxi` \
                     MIUI 14 build shape"
                .to_string(),
            legal_note: "Fabricating a specific-OEM identity requires counsel (trademark / \
                         TAC-reprogramming statutes). Reference/dev fixture only."
                .to_string(),
        },
        build: BuildIdentity {
            brand: "Xiaomi".to_string(),
            product_name: "fuxi".to_string(),
            device: "fuxi".to_string(),
            model: "2211133C".to_string(),
            manufacturer: "Xiaomi".to_string(),
            board: "fuxi".to_string(),
            bootloader: "unknown".to_string(),
            hardware: "qcom".to_string(),
            host: "c5-miui-ota-bd131.bj".to_string(),
            user: "builder".to_string(),
            tags: "release-keys".to_string(),
            build_type: "user".to_string(),
            build_id: "TKQ1.220829.002".to_string(),
            incremental: "V14.0.6.0.TMCCNXM".to_string(),
            release: "13".to_string(),
            sdk: 33,
            security_patch: "2023-04-01".to_string(),
            display_id: "TKQ1.220829.002".to_string(),
        },
        soc: SocProfile {
            soc_manufacturer: "QTI".to_string(),
            soc_model: "SM8550".to_string(),
            board_platform: "kalama".to_string(),
            midr_clusters: vec![
                // Snapdragon 8 Gen 2 is 1+2+2+3: 1x Cortex-X3 prime + 2x Cortex-A715 + 2x
                // Cortex-A710 (the gold tier really is two different µarchs) + 3x Cortex-A510
                // silver. All stock ARM cores, so implementer 0x41 — not custom-Kryo 0x51.
                MidrCluster {
                    part_name: "Cortex-X3 (prime)".to_string(),
                    implementer: 0x41,
                    part: 0xd4e,
                    variant: 0x1,
                    revision: 0x0,
                    core_count: 1,
                    max_freq_khz: 3_187_200,
                },
                MidrCluster {
                    part_name: "Cortex-A715 (gold)".to_string(),
                    implementer: 0x41,
                    part: 0xd4d,
                    variant: 0x1,
                    revision: 0x0,
                    core_count: 2,
                    max_freq_khz: 2_803_200,
                },
                MidrCluster {
                    part_name: "Cortex-A710 (gold)".to_string(),
                    implementer: 0x41,
                    part: 0xd47,
                    variant: 0x1,
                    revision: 0x0,
                    core_count: 2,
                    max_freq_khz: 2_803_200,
                },
                MidrCluster {
                    part_name: "Cortex-A510 (silver)".to_string(),
                    implementer: 0x41,
                    part: 0xd46,
                    variant: 0x1,
                    revision: 0x0,
                    core_count: 3,
                    max_freq_khz: 2_016_000,
                },
            ],
            socinfo: Socinfo {
                family: "Snapdragon".to_string(),
                machine: "fuxi".to_string(),
                // SM8550 (kalama) in drivers/soc/qcom/socinfo.c.
                soc_id: 519,
                revision: "2.0".to_string(),
                serial_number: String::new(),
                foundry_id: 0,
                chip_family: 0,
                raw_device_family: 0,
                raw_device_number: 0,
                hardware_platform: "MTP".to_string(),
                pmic_model: String::new(),
            },
        },
        abi: AbiProfile {
            abilist: "arm64-v8a,armeabi-v7a,armeabi".to_string(),
            abilist32: "armeabi-v7a,armeabi".to_string(),
            abilist64: "arm64-v8a".to_string(),
        },
        // Adreno 740 — a Vulkan 1.3 device (the SM8250-era Adreno 650 topped out at 1.1 on its
        // shipping driver). This is the level the guest must be able to claim for a Vulkan 1.3
        // host renderer (gfxstream -> KosmicKrisp) to be a coherent story.
        gpu: GpuProfile {
            gl_vendor: "Qualcomm".to_string(),
            gl_renderer: "Adreno (TM) 740".to_string(),
            gl_version: "OpenGL ES 3.2 V@0744.0 (GIT@a0d5f2b, Ie4a2f9d70e, 1675814076)".to_string(),
            vk_device_name: "Adreno (TM) 740".to_string(),
            vk_vendor_id: 0x5143, // Qualcomm
            vk_device_type: VkDeviceType::IntegratedGpu,
            vk_driver_id: "VK_DRIVER_ID_QUALCOMM_PROPRIETARY".to_string(),
            ro_hardware_egl: "adreno".to_string(),
        },
        // The real fuxi panel: 6.36" (162 mm) 1080x2400 AMOLED, declared at 440 dpi — which yields
        // 393 dp of width, exactly what a real Xiaomi 13 reports, against a ~414 ppi physical
        // density. 60 Hz, not the panel's 120: the refresh rate the *host* can actually present is
        // the honest value, and it is the one the EDID's detailed timing has to agree with.
        display: DisplayProfile {
            width_px: 1080,
            height_px: 2400,
            density_dpi: 440,
            refresh_hz: 60,
            diagonal_mm: 162,
        },
        sensors: vec![
            SensorSpec {
                sensor_type: "accelerometer".to_string(),
                name: "LSM6DSO Accelerometer".to_string(),
                vendor: "STMicroelectronics".to_string(),
                max_range: 78.4532,
                resolution: 0.002_396_85,
                power: 0.15,
                min_delay: 2000,
            },
            SensorSpec {
                sensor_type: "magnetic_field".to_string(),
                name: "AK09918 Magnetometer".to_string(),
                vendor: "AKM".to_string(),
                max_range: 4912.0,
                resolution: 0.15,
                power: 1.1,
                min_delay: 10000,
            },
        ],
        // WiFi-only tablet posture is the default (deletes the telephony surface).
        telephony: TelephonyProfile::WifiOnly,
        verified_boot: VerifiedBoot {
            verifiedbootstate: "green".to_string(),
            flash_locked: true,
            device_state: "locked".to_string(),
            veritymode: "enforcing".to_string(),
        },
        network: NetworkProfile {
            mac_oui: "F0:B4:29".to_string(), // Xiaomi OUI
            interfaces: vec!["wlan0".to_string(), "p2p0".to_string()],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_profile_yields_the_known_xiaomi_fingerprint() {
        assert_eq!(
            reference_profile().fingerprint(),
            "Xiaomi/fuxi/fuxi:13/TKQ1.220829.002/V14.0.6.0.TMCCNXM:user/release-keys"
        );
    }
}
