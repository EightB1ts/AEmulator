//! `DeviceConfig` assembly — the honest emulated + Partial + Planned inventory
//! (DESIGN.md §2.4). This is deterministic and REAL even in BUILD 1: it does not
//! fabricate a capability, it just reports what the `aemulator-hvf` VMM provides
//! today. A profile's `render_mode`/telephony toggles a couple of entries.
//!
//! Each entry carries TWO orthogonal axes: `status` (the honesty tier — is this
//! real?) and `group` (the subsystem — what does it do?). The Hardware tab groups
//! on `group` and renders `status` as a per-item dot, so a subsystem's real and
//! aspirational parts stay adjacent instead of being scattered across three
//! status-sorted lists.

use crate::types::{
    DeviceConfig, DeviceEntry, DeviceGroup, DeviceKind, DeviceStatus, EmulatorId, EmulatorProfile,
    RenderMode,
};

fn entry(
    kind: DeviceKind,
    label: &str,
    status: DeviceStatus,
    group: DeviceGroup,
    detail: &str,
    controllable: bool,
) -> DeviceEntry {
    DeviceEntry {
        kind,
        label: label.to_string(),
        status,
        group,
        detail: detail.to_string(),
        controllable,
    }
}

/// Build the full device inventory for a profile.
///
/// `net_available` is probed from the emulator binary (`spawn::binary_has_net`), not assumed. This
/// entry used to be a hardcoded `Emulated` with the caveat buried in its prose ("built --features
/// net; else enumerate-only") — which broke this module's one rule: report what the VMM provides,
/// never fabricate. A `--features gfxstream`-only binary was reported as having real NAT while the
/// guest's packets went nowhere.
pub fn device_config_for(profile: &EmulatorProfile, net_available: bool) -> DeviceConfig {
    use DeviceGroup as G;
    use DeviceKind as K;
    use DeviceStatus::*;

    let gpu3d_status = match profile.render_mode {
        RenderMode::Gpu3d => Emulated,
        // Present in the binary but not selected for this profile's launch.
        RenderMode::SwRender | RenderMode::Headless => Partial,
    };
    // Honest per-binary status: real NAT only when libslirp is actually linked in. The stub is
    // Partial rather than Planned — it does enumerate and link up, it just carries no traffic.
    let (net_status, net_detail) = if net_available {
        (Emulated, "Real NAT (guest 10.0.2.15/24, gw 10.0.2.2, DNS 10.0.2.3) + adb-over-TCP via the per-VM host port-forward.")
    } else {
        (Partial, "Enumerate-only stub NIC — links up and takes an IP but drops all traffic; adb cannot connect. Rebuild the emulator binary with `--features net`.")
    };

    let gpu3d_detail = match profile.render_mode {
        RenderMode::Gpu3d => "GPU 3D (Metal via gfxstream) — active for this profile",
        RenderMode::SwRender => "GPU 3D available but this profile uses SwiftShader (SWRENDER=1)",
        RenderMode::Headless => "GPU 3D available but this profile boots headless",
    };

    let mut devices = vec![
        // ---- Emulated ----
        entry(K::Cpu, "CPU identity spoof (MIDR)", Emulated, G::Compute,
            "big.LITTLE MIDR writes; blocks the Apple 0x61 implementer leak.", false),
        entry(K::Rng, "Entropy (virtio-rng)", Emulated, G::Compute,
            "Host-seeded randomness for the guest.", false),
        entry(K::Gpu2d, "GPU 2D scanout (virtio-gpu)", Emulated, G::Graphics,
            "Always on. EDID 1024×768 framebuffer.", false),
        entry(K::Gpu3d, "GPU 3D (Metal via gfxstream)", gpu3d_status, G::Graphics, gpu3d_detail, false),
        entry(K::BlockPrimary, "Storage: system (GPT composite → /dev/vda)", Emulated, G::Storage,
            "Primary block device backing the Cuttlefish image.", false),
        entry(K::Input, "Touch + keyboard (virtio-input)", Emulated, G::Io,
            "The tap / swipe / text / key surface.", true),
        entry(K::Console, "Serial console (12 HVC channels)", Emulated, G::Io,
            "Backs launch / sh over the control plane.", true),
        entry(K::Net, "Network NAT (libslirp)", net_status, G::Io, net_detail, false),
        // ---- Partial ----
        entry(K::BlockExtra, "Extra disks (fixed files)", Partial, G::Storage,
            "--disk only; no removable / SD card model.", false),
        entry(K::Keymint, "KeyMint (software stub, NOT a TEE)", Partial, G::Security,
            "CBOR responder brings keystore2 up; Gatekeeper / oemlock logged-only.", false),
        entry(K::VerifiedBoot, "Verified boot state (metadata only)", Partial, G::Security,
            "Static verifiedbootstate=green; no real attestation chain.", false),
        entry(K::Sensors, "Sensors (declared, no live data)", Partial, G::Sensors,
            "Profile advertises accel / magnetometer to getSensorList(); no HAL delivers values.", false),
        // ---- Planned (placeholder UI only) ----
        entry(K::Vsock, "HAL transport (vsock) — not implemented", Planned, G::Compute,
            "Blocks real camera / other HALs.", false),
        entry(K::Audio, "Audio playback / capture — planned", Planned, G::Io,
            "virtio-snd → CoreAudio.", false),
        entry(K::Clipboard, "Host↔guest clipboard — planned", Planned, G::Io,
            "No clipboard bridge.", false),
        entry(K::Vibration, "Vibration / haptics — planned", Planned, G::Io,
            "No vibrator HAL.", false),
        entry(K::Gps, "GPS / location — planned", Planned, G::Sensors,
            "The README 'gps' verb does not exist yet.", false),
        entry(K::Camera, "Camera — planned", Planned, G::Sensors,
            "Needs vsock + a camera HAL.", false),
        entry(K::Battery, "Battery / power — planned", Planned, G::Sensors,
            "No power HAL wired.", false),
        entry(K::Storage, "Removable storage / SD card — planned", Planned, G::Storage,
            "No hot-plug block model.", false),
        entry(K::Bluetooth, "Bluetooth — planned", Planned, G::Sensors, "No BT transport.", false),
        entry(K::Nfc, "NFC — planned", Planned, G::Sensors, "No NFC transport.", false),
    ];

    // Telephony only appears when the profile declares a radio (Partial: metadata only).
    if profile.device_identity.telephony.is_some() {
        devices.push(entry(
            K::Telephony,
            "Telephony (metadata only)",
            Partial,
            G::Sensors,
            "Advertises IMEI / operator strings; no live radio, no call / SMS path.",
            false,
        ));
    }

    DeviceConfig {
        emulator_id: profile.id.clone() as EmulatorId,
        devices,
    }
}
