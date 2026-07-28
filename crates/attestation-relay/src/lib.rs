//! # attestation-relay
//!
//! **OPTIONAL — Phase-5, per-flow, fragile, opt-in.** Not part of the v1 promise. Ship it **behind
//! explicit per-app enablement**, only for legitimate automation targets, and treat every success as
//! fragile and regression-prone.
//!
//! This crate does **not** make the VM attestable. Hardware-backed attestation (Play Integrity
//! DEVICE/STRONG, Android Key Attestation `TrustedEnvironment`/`StrongBox`, Widevine L1) is a
//! cryptographic wall the VMM can never pass — keys are fused into real TEE/StrongBox silicon at the
//! factory and chain to Google's Hardware Attestation Root (OID `1.3.6.1.4.1.11129.2.1.17`). What
//! this broker does is **borrow a real rooted device's genuine hardware attestation for a single
//! request**: it forwards `{nonce, package}` to a device in a pool, which produces a *genuine* token,
//! and returns it.
//!
//! ## Two relay modes ([`RelayMode`])
//!
//! - **[`PlayIntegrityTokenRelay`](RelayMode::PlayIntegrityTokenRelay)** — forward the app's
//!   `{nonce, package}` to a real device running the same app; the device produces a genuine Play
//!   Integrity token whose `deviceIntegrity.deviceRecognitionVerdict` is `MEETS_DEVICE_INTEGRITY`
//!   (or `MEETS_STRONG_INTEGRITY`); return it verbatim.
//! - **[`KeystoreSignProxy`](RelayMode::KeystoreSignProxy)** — a remote-HSM proxy: the key is
//!   generated inside the real device's TEE/StrongBox and every sign/decrypt op is proxied to it, so
//!   the returned cert chain carries the *genuine* hardware `RootOfTrust` with
//!   `attestationSecurityLevel = TrustedEnvironment(1)`/`StrongBox(2)` — never the `Software(0)` a
//!   local keystore would emit.
//!
//! ## Honest constraints (design gates — treat any success as fragile)
//!
//! - **Correlation:** works only when the app's server checks the baseline (verdict +
//!   `requestDetails.requestHash`/nonce + `appIntegrity.appRecognitionVerdict == PLAY_RECOGNIZED` +
//!   `accountDetails.appLicensingVerdict == LICENSED`). It **fails** when the server correlates the
//!   attested device against other device-bound signals it collected from our VM (Build props, a key
//!   attestation, TLS/JA3, datacenter ASN) — the mismatch is visible.
//! - **Rate / recall:** one real device serving many VM sessions trips Play Integrity's
//!   `recentDeviceActivity` (LEVEL_1..LEVEL_4 by requests/hour) and `deviceRecall` (persists across
//!   reinstall and factory reset). `environmentDetails.appAccessRiskVerdict.appsDetected` can also
//!   flag the automation directly (`KNOWN_CONTROLLING`/`UNKNOWN_CONTROLLING`/`*_CAPTURING`).
//! - **Provisioning churn:** devices launching with Android 16 are RKP-only (no factory keys), and a
//!   new attestation root (`CN=Key Attestation CA1, OU=Android, O=Google LLC`, ECDSA P-384) begins
//!   Feb 1 2026 — the pool's real devices must stay current or their chains fail verification.
//! - **Latency:** the relay adds round-trip latency a timing check can see.
//! - **Legal:** device-farm ToS + potential fraud overlap.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors from the relay broker.
#[derive(Debug, Error)]
pub enum RelayError {
    /// No real device was available in the pool to serve the request.
    #[error("no real device available in the pool")]
    NoDeviceAvailable,
    /// The relay to a specific device failed.
    #[error("relay to device `{device_id}` failed: {detail}")]
    RelayFailed { device_id: String, detail: String },
}

/// Which flavour of attestation the broker relays. See the module docs for the honest per-mode
/// constraints. Defaults to [`PlayIntegrityTokenRelay`](RelayMode::PlayIntegrityTokenRelay).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayMode {
    /// Relay a genuine Play Integrity token (`deviceIntegrity.deviceRecognitionVerdict =
    /// MEETS_DEVICE_INTEGRITY`) produced by a real device for `{nonce, package}`.
    #[default]
    PlayIntegrityTokenRelay,
    /// Proxy keystore sign/attest ops to a real device's TEE/StrongBox, returning its genuine
    /// hardware-backed cert chain (`attestationSecurityLevel` = `TrustedEnvironment`/`StrongBox`,
    /// never `Software`).
    KeystoreSignProxy,
}

/// A request to relay: the app's attestation `nonce` and the `package` it is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayRequest {
    /// The attestation nonce/challenge issued to the guest app.
    pub nonce: String,
    /// The requesting app's package name.
    pub package: String,
}

/// The relayed result: a genuine token produced by a real device, plus which device produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayResponse {
    /// The genuine attestation token (e.g. a Play Integrity token) from the real device.
    pub token: String,
    /// The pooled device that produced it (for correlation/audit).
    pub device_id: String,
}

/// A lease on a real device from the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceHandle {
    pub device_id: String,
}

/// A pool of real rooted devices the broker leases from.
pub trait DevicePool {
    /// Acquire a device to serve a request.
    fn acquire(&mut self) -> Result<DeviceHandle, RelayError>;
    /// Return a device to the pool.
    fn release(&mut self, handle: DeviceHandle);
    /// How many devices are currently available.
    fn available(&self) -> usize;
}

/// An always-empty pool — a placeholder for the real-device pool manager. `acquire` always fails
/// with [`RelayError::NoDeviceAvailable`], which keeps the broker safe to construct with no fleet.
#[derive(Debug, Default)]
pub struct NullPool;

impl DevicePool for NullPool {
    fn acquire(&mut self) -> Result<DeviceHandle, RelayError> {
        Err(RelayError::NoDeviceAvailable)
    }
    fn release(&mut self, _handle: DeviceHandle) {}
    fn available(&self) -> usize {
        0
    }
}

/// A testable in-memory pool of named mock devices. Leasing pops a device (decrementing
/// [`available`](DevicePool::available)); [`release`](DevicePool::release) returns it. It models the
/// lease accounting faithfully so the broker's acquire → relay → release flow is exercisable without
/// a real fleet — it produces **no** genuine attestation (see [`RelayBroker::relay`]).
#[derive(Debug, Clone, Default)]
pub struct MockDevicePool {
    free: Vec<String>,
    leased: usize,
}

impl MockDevicePool {
    /// A pool seeded with explicit device ids.
    pub fn new(device_ids: impl IntoIterator<Item = String>) -> Self {
        Self {
            free: device_ids.into_iter().collect(),
            leased: 0,
        }
    }

    /// A pool of `n` synthetically-named devices (`mock-device-0`..`mock-device-{n-1}`).
    pub fn with_size(n: usize) -> Self {
        Self::new((0..n).map(|i| format!("mock-device-{i}")))
    }

    /// How many devices are currently leased out.
    pub fn leased(&self) -> usize {
        self.leased
    }
}

impl DevicePool for MockDevicePool {
    fn acquire(&mut self) -> Result<DeviceHandle, RelayError> {
        match self.free.pop() {
            Some(device_id) => {
                self.leased += 1;
                Ok(DeviceHandle { device_id })
            }
            None => Err(RelayError::NoDeviceAvailable),
        }
    }

    fn release(&mut self, handle: DeviceHandle) {
        self.free.push(handle.device_id);
        self.leased = self.leased.saturating_sub(1);
    }

    fn available(&self) -> usize {
        self.free.len()
    }
}

/// The per-flow relay broker. Holds a [`DevicePool`] + a [`RelayMode`]; [`relay`](Self::relay) leases
/// a device and forwards the request to it.
pub struct RelayBroker {
    pool: Box<dyn DevicePool + Send>,
    mode: RelayMode,
}

impl RelayBroker {
    /// Construct a broker over a device pool using the default [`RelayMode`]
    /// ([`RelayMode::PlayIntegrityTokenRelay`]).
    pub fn new(pool: Box<dyn DevicePool + Send>) -> Self {
        Self::with_mode(pool, RelayMode::default())
    }

    /// Construct a broker over a device pool with an explicit [`RelayMode`].
    pub fn with_mode(pool: Box<dyn DevicePool + Send>, mode: RelayMode) -> Self {
        Self { pool, mode }
    }

    /// The relay mode this broker is operating in.
    pub fn mode(&self) -> RelayMode {
        self.mode
    }

    /// Switch the relay mode.
    pub fn set_mode(&mut self, mode: RelayMode) {
        self.mode = mode;
    }

    /// Devices currently available in the pool.
    pub fn available(&self) -> usize {
        self.pool.available()
    }

    /// Relay one attestation request. Acquires a device first (so a broker with no fleet fails fast
    /// with [`RelayError::NoDeviceAvailable`] before any forwarding is attempted), forwards the
    /// request, then returns the device to the pool.
    ///
    /// **Phase-5 seam.** The forwarding half — sending `{nonce, package}` to the leased device's
    /// on-device agent and awaiting its *genuine* hardware-backed token — is Phase-5 work against a
    /// real fleet. Against a [`MockDevicePool`] this synthesizes a **clearly-labelled placeholder**
    /// token (never a genuine attestation) so the acquire → forward → release plumbing and the
    /// [`RelayMode`] branch are exercisable offline.
    pub fn relay(&mut self, request: RelayRequest) -> Result<RelayResponse, RelayError> {
        let handle = self.pool.acquire()?;
        let device_id = handle.device_id.clone();
        let token = self.synthesize_placeholder_token(&request, &device_id);
        // Always return the device to the pool, even on the mock path, so lease accounting balances.
        self.pool.release(handle);
        Ok(RelayResponse { token, device_id })
    }

    /// Build the placeholder token for the mock path. A real broker replaces this with the device's
    /// genuine token/cert-chain; the prefix makes the mock impossible to mistake for a real one.
    fn synthesize_placeholder_token(&self, request: &RelayRequest, device_id: &str) -> String {
        match self.mode {
            RelayMode::PlayIntegrityTokenRelay => format!(
                "MOCK-NOT-GENUINE::play_integrity_token_relay::device={device_id}::package={}::nonce={}::deviceRecognitionVerdict=MEETS_DEVICE_INTEGRITY",
                request.package, request.nonce
            ),
            RelayMode::KeystoreSignProxy => format!(
                "MOCK-NOT-GENUINE::keystore_sign_proxy::device={device_id}::package={}::nonce={}::attestationSecurityLevel=TrustedEnvironment",
                request.package, request.nonce
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> RelayRequest {
        RelayRequest {
            nonce: "abc".to_string(),
            package: "com.example.app".to_string(),
        }
    }

    #[test]
    fn null_pool_reports_no_devices() {
        let mut pool = NullPool;
        assert_eq!(pool.available(), 0);
        assert!(matches!(pool.acquire(), Err(RelayError::NoDeviceAvailable)));
    }

    #[test]
    fn broker_over_empty_pool_fails_fast_without_touching_the_relay_body() {
        let mut broker = RelayBroker::new(Box::new(NullPool));
        assert_eq!(broker.available(), 0);
        let err = broker.relay(req()).unwrap_err();
        // acquire() fails before the forwarding body is reached.
        assert!(matches!(err, RelayError::NoDeviceAvailable));
    }

    #[test]
    fn mock_pool_lease_accounting() {
        let mut pool = MockDevicePool::with_size(2);
        assert_eq!(pool.available(), 2);
        assert_eq!(pool.leased(), 0);
        let h = pool.acquire().unwrap();
        assert_eq!(pool.available(), 1);
        assert_eq!(pool.leased(), 1);
        pool.release(h);
        assert_eq!(pool.available(), 2);
        assert_eq!(pool.leased(), 0);
    }

    #[test]
    fn mock_pool_exhaustion_yields_no_device_available() {
        let mut pool = MockDevicePool::with_size(1);
        let _h = pool.acquire().unwrap();
        assert!(matches!(pool.acquire(), Err(RelayError::NoDeviceAvailable)));
    }

    #[test]
    fn broker_defaults_to_play_integrity_mode() {
        let broker = RelayBroker::new(Box::new(MockDevicePool::with_size(1)));
        assert_eq!(broker.mode(), RelayMode::PlayIntegrityTokenRelay);
    }

    #[test]
    fn relay_over_mock_pool_returns_labelled_token_and_releases_device() {
        let mut broker = RelayBroker::new(Box::new(MockDevicePool::with_size(1)));
        assert_eq!(broker.available(), 1);
        let resp = broker.relay(req()).unwrap();
        // The device was leased and returned — availability is restored.
        assert_eq!(broker.available(), 1);
        assert!(!resp.device_id.is_empty());
        // The mock token is unmistakably a placeholder and carries the PI verdict + request context.
        assert!(resp.token.starts_with("MOCK-NOT-GENUINE::"));
        assert!(resp.token.contains("play_integrity_token_relay"));
        assert!(resp.token.contains("MEETS_DEVICE_INTEGRITY"));
        assert!(resp.token.contains("com.example.app"));
        assert!(resp.token.contains(&resp.device_id));
    }

    #[test]
    fn keystore_mode_is_reflected_in_the_relayed_token() {
        let mut broker = RelayBroker::with_mode(
            Box::new(MockDevicePool::with_size(1)),
            RelayMode::KeystoreSignProxy,
        );
        assert_eq!(broker.mode(), RelayMode::KeystoreSignProxy);
        let resp = broker.relay(req()).unwrap();
        assert!(resp.token.contains("keystore_sign_proxy"));
        assert!(resp
            .token
            .contains("attestationSecurityLevel=TrustedEnvironment"));
        assert!(!resp.token.contains("MEETS_DEVICE_INTEGRITY"));
    }

    #[test]
    fn set_mode_switches_the_relay_flavour() {
        let mut broker = RelayBroker::new(Box::new(MockDevicePool::with_size(1)));
        broker.set_mode(RelayMode::KeystoreSignProxy);
        assert_eq!(broker.mode(), RelayMode::KeystoreSignProxy);
    }

    #[test]
    fn relay_mode_serde_roundtrip_is_snake_case() {
        let json = serde_json::to_string(&RelayMode::KeystoreSignProxy).unwrap();
        assert_eq!(json, "\"keystore_sign_proxy\"");
        let back: RelayMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RelayMode::KeystoreSignProxy);
    }
}
