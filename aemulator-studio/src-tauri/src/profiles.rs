//! `EmulatorProfile` persistence — a single `profiles.json` under the app config
//! dir. On first run (no file) the store is seeded with two example profiles so
//! the Dashboard isn't empty, then immediately persisted.

use std::path::{Path, PathBuf};

use crate::types::{DeviceIdentity, EmulatorProfile, RenderMode, SelinuxMode, TelephonyMeta};

/// The profiles file name inside the app config dir.
pub const PROFILES_FILE: &str = "profiles.json";

/// Two example profiles for first-run so the Dashboard renders with content.
pub fn seed() -> Vec<EmulatorProfile> {
    vec![
        EmulatorProfile {
            id: "11111111-1111-4111-8111-111111111111".into(),
            name: "Cuttlefish · SwRender".into(),
            base_image: "cf64".into(),
            mem_mb: 4096,
            cpus: 2,
            // SwRender is the stable, window-less default (SWRENDER=1).
            render_mode: RenderMode::SwRender,
            selinux: SelinuxMode::Permissive,
            extra_cmdline: String::new(),
            device_identity: DeviceIdentity {
                soc: "Snapdragon 8 Gen 2 / SM8550".into(),
                manufacturer: "Xiaomi".into(),
                model: "fuxi".into(),
                advertised_sensors: vec!["accelerometer".into(), "magnetometer".into()],
                telephony: Some(TelephonyMeta {
                    imei: "350000000000001".into(),
                    operator: "Verizon".into(),
                    mcc_mnc: "311480".into(),
                }),
            },
            created_at: 1_753_000_000_000,
        },
        EmulatorProfile {
            id: "22222222-2222-4222-8222-222222222222".into(),
            name: "Cuttlefish · Metal GPU".into(),
            base_image: "cf64".into(),
            mem_mb: 6144,
            cpus: 4,
            render_mode: RenderMode::Gpu3d,
            selinux: SelinuxMode::Permissive,
            extra_cmdline: String::new(),
            device_identity: DeviceIdentity {
                soc: "Snapdragon 888 / SM8350".into(),
                manufacturer: "Google".into(),
                model: "redfin".into(),
                advertised_sensors: vec!["accelerometer".into()],
                telephony: None,
            },
            created_at: 1_753_100_000_000,
        },
    ]
}

/// Full path to the profiles file inside `config_dir`.
pub fn profiles_path(config_dir: &Path) -> PathBuf {
    config_dir.join(PROFILES_FILE)
}

/// Load persisted profiles. On a missing file, seed + persist and return the
/// seed. On a corrupt file, fall back to the seed WITHOUT overwriting (so a
/// hand-edit mistake is recoverable) — the error is surfaced by the caller's log.
pub fn load(config_dir: &Path) -> Vec<EmulatorProfile> {
    let path = profiles_path(config_dir);
    match std::fs::read_to_string(&path) {
        Ok(body) => match serde_json::from_str::<Vec<EmulatorProfile>>(&body) {
            Ok(profiles) => profiles,
            Err(e) => {
                eprintln!("[studio] profiles.json parse error ({e}); using seed (not overwriting)");
                seed()
            }
        },
        Err(_) => {
            let seeded = seed();
            let _ = save(config_dir, &seeded);
            seeded
        }
    }
}

/// Persist profiles atomically (write temp + rename).
pub fn save(config_dir: &Path, profiles: &[EmulatorProfile]) -> Result<(), String> {
    std::fs::create_dir_all(config_dir)
        .map_err(|e| format!("mkdir {}: {e}", config_dir.display()))?;
    let body = serde_json::to_string_pretty(profiles).map_err(|e| format!("serialize: {e}"))?;
    let path = profiles_path(config_dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}
