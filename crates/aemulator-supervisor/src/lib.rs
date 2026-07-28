//! # aemulator-supervisor
//!
//! Device **lifecycle + orchestration**. The [`Supervisor`] owns one device's [`DeviceProfile`],
//! drives the [`aemulator_vmm`] backend to produce that device's launcher args + device tree, and
//! exposes the [`aemulator_control`] plane (observe/act) to a client. It is the single object a host
//! process (or the future control-service shell) holds per running device.
//!
//! ## Lifecycle state machine
//!
//! ```text
//!     Created ──boot()──▶ Booting ──▶ Ready ──shutdown()──▶ Stopped
//!                            │
//!                            └── launch error ──▶ Failed
//! ```
//!
//! [`Supervisor::boot`] transitions `Created → Booting`, asks the [`Launcher`] to bring the device
//! up, and lands on [`DeviceState::Ready`] on success or [`DeviceState::Failed`] on error.
//! observe/act are refused with [`SupervisorError::NotReady`] unless the device is
//! [`DeviceState::Ready`].
//!
//! ## The launcher seam (how a real VMM drops in)
//!
//! Booting is delegated to a [`Launcher`]. The default [`MockLauncher`]:
//!   1. builds the hand-authored device tree (proving the identity → dtb path, so a bad profile
//!      surfaces as a boot failure, not a mid-session crash),
//!   2. generates the VMM launcher argument vector via [`aemulator_vmm::qemu_hvf_args`] (the
//!      identity → vmm arg-gen path) and stashes it on the [`Launched`] result,
//!   3. attaches an in-memory [`MockController`] as the control channel.
//!
//! It does **not** spawn a real VM. A production launcher implements the same [`Launcher`] trait:
//! `exec` the args from step 2, wait for the guest control channel, and return a real
//! [`DeviceController`] over vsock — all **without changing [`Supervisor`]'s public API**. Inject it
//! with [`Supervisor::with_launcher`].

use aemulator_control::{Action, DeviceController, MockController, Observation};
use aemulator_identity::DeviceProfile;
use aemulator_vmm::BootAssets;
use thiserror::Error;

/// The device lifecycle state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// Constructed, not yet booted.
    Created,
    /// `boot()` is in progress.
    Booting,
    /// Booted and serving observe/act.
    Ready,
    /// Cleanly shut down.
    Stopped,
    /// Boot or a lifecycle op failed.
    Failed,
}

/// Errors from supervisor operations.
#[derive(Debug, Error)]
pub enum SupervisorError {
    /// An observe/act was attempted while the device was not [`DeviceState::Ready`].
    #[error("device not ready (state = {0:?})")]
    NotReady(DeviceState),
    /// The control plane returned an error.
    #[error(transparent)]
    Control(#[from] aemulator_control::ControlError),
    /// The VMM backend returned an error.
    #[error(transparent)]
    Vmm(#[from] aemulator_vmm::VmmError),
    /// A real [`Launcher`] failed to bring the device up (spawn/boot/connect). Carries the
    /// backend's message. The default [`MockLauncher`] never produces it.
    #[error("launch failed: {0}")]
    Launch(String),
}

/// What a [`Launcher`] produces when it brings a device up: the VMM launcher argument vector it used
/// (or would exec) plus the live control channel to drive the guest.
pub struct Launched {
    /// The generated VMM launcher args (e.g. the `qemu-system-aarch64` vector). A real launcher has
    /// already exec'd these; the mock merely generated them so the identity → vmm path is proven.
    pub launch_args: Vec<String>,
    /// The control channel for observe/act against the (now-running) guest.
    pub controller: Box<dyn DeviceController + Send>,
}

/// The seam a device launcher implements. The default is [`MockLauncher`]; a production launcher
/// spawns the real VMM from the generated args and returns a real [`DeviceController`] over the guest
/// control channel — dropping in without any change to [`Supervisor`]'s public API.
pub trait Launcher: Send {
    /// Bring up the device described by `profile` from `assets`, returning the launcher args used and
    /// the live control channel.
    fn launch(
        &self,
        profile: &DeviceProfile,
        assets: &BootAssets,
    ) -> Result<Launched, SupervisorError>;
}

/// The default launcher: builds the device tree + generates the VMM launcher args (proving the
/// identity → dtb → vmm arg-gen path) and attaches an in-memory [`MockController`]. It does **not**
/// spawn a real VM.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockLauncher;

impl Launcher for MockLauncher {
    fn launch(
        &self,
        profile: &DeviceProfile,
        assets: &BootAssets,
    ) -> Result<Launched, SupervisorError> {
        // Materialize the device tree now so an identity/coherence error surfaces at boot, not
        // mid-session. Propagates as `SupervisorError::Vmm` (→ `DeviceState::Failed`).
        let _dtb = aemulator_vmm::build_device_tree(profile)?;
        // Wire VMM arg-gen: a real launcher would `exec` this vector; we stash it for inspection.
        let launch_args = aemulator_vmm::qemu_hvf_args(profile, assets);
        Ok(Launched {
            launch_args,
            controller: Box::new(MockController::new()),
        })
    }
}

/// One device's supervisor: identity + lifecycle state + the generated launcher args + the control
/// channel produced by the [`Launcher`].
pub struct Supervisor {
    profile: DeviceProfile,
    state: DeviceState,
    launcher: Box<dyn Launcher>,
    launch_args: Vec<String>,
    controller: Box<dyn DeviceController + Send>,
}

impl Supervisor {
    /// Create a supervisor for `profile` backed by the default [`MockLauncher`]. The device starts in
    /// [`DeviceState::Created`].
    pub fn new(profile: DeviceProfile) -> Self {
        Self::with_launcher(profile, Box::new(MockLauncher))
    }

    /// Create a supervisor for `profile` backed by a caller-supplied [`Launcher`]. This is the
    /// drop-in point for the real VMM launcher (or a test double) — the public lifecycle API is
    /// identical either way.
    pub fn with_launcher(profile: DeviceProfile, launcher: Box<dyn Launcher>) -> Self {
        Self {
            profile,
            state: DeviceState::Created,
            launcher,
            launch_args: Vec::new(),
            // A placeholder controller until `boot()` attaches the launcher's real one. observe/act
            // are gated on `DeviceState::Ready`, so this is never reachable before boot.
            controller: Box::new(MockController::new()),
        }
    }

    /// The device's identity profile.
    pub fn profile(&self) -> &DeviceProfile {
        &self.profile
    }

    /// The current lifecycle state.
    pub fn state(&self) -> DeviceState {
        self.state
    }

    /// The VMM launcher argument vector generated at the last successful [`boot`](Self::boot). Empty
    /// before a device has booted.
    pub fn launch_args(&self) -> &[String] {
        &self.launch_args
    }

    /// Boot the device from `assets`.
    ///
    /// Transitions `→ Booting`, delegates to the [`Launcher`], and lands on [`DeviceState::Ready`]
    /// (attaching the launcher's control channel + generated args) or [`DeviceState::Failed`] on
    /// error. On failure the previous control channel and args are left untouched and the error is
    /// returned to the caller.
    pub fn boot(&mut self, assets: &BootAssets) -> Result<(), SupervisorError> {
        self.state = DeviceState::Booting;
        match self.launcher.launch(&self.profile, assets) {
            Ok(launched) => {
                self.launch_args = launched.launch_args;
                self.controller = launched.controller;
                self.state = DeviceState::Ready;
                Ok(())
            }
            Err(e) => {
                self.state = DeviceState::Failed;
                Err(e)
            }
        }
    }

    /// Capture the current [`Observation`]. Errors with [`SupervisorError::NotReady`] unless the
    /// device is [`DeviceState::Ready`].
    pub fn observe(&self) -> Result<Observation, SupervisorError> {
        if self.state != DeviceState::Ready {
            return Err(SupervisorError::NotReady(self.state));
        }
        Ok(self.controller.observe()?)
    }

    /// Perform one [`Action`] and return the post-action observation. Errors with
    /// [`SupervisorError::NotReady`] unless the device is [`DeviceState::Ready`]. A control-plane
    /// error (e.g. a stale generation) is surfaced as [`SupervisorError::Control`] and leaves the
    /// lifecycle state as [`DeviceState::Ready`] (the device is still up; only the action failed).
    pub fn act(&mut self, action: Action) -> Result<Observation, SupervisorError> {
        if self.state != DeviceState::Ready {
            return Err(SupervisorError::NotReady(self.state));
        }
        Ok(self.controller.act(action)?)
    }

    /// Shut the device down. Idempotent; leaves the device in [`DeviceState::Stopped`].
    pub fn shutdown(&mut self) -> Result<(), SupervisorError> {
        self.state = DeviceState::Stopped;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aemulator_control::ControlError;
    use aemulator_identity::profiles::reference_profile;
    use aemulator_vmm::VmmError;

    #[test]
    fn lifecycle_transitions_created_to_ready_to_stopped() {
        let mut sup = Supervisor::new(reference_profile());
        assert_eq!(sup.state(), DeviceState::Created);
        sup.boot(&BootAssets::default()).unwrap();
        assert_eq!(sup.state(), DeviceState::Ready);
        sup.shutdown().unwrap();
        assert_eq!(sup.state(), DeviceState::Stopped);
    }

    #[test]
    fn observe_before_boot_is_not_ready() {
        let sup = Supervisor::new(reference_profile());
        match sup.observe() {
            Err(SupervisorError::NotReady(DeviceState::Created)) => {}
            other => panic!("expected NotReady(Created), got {other:?}"),
        }
    }

    #[test]
    fn observe_and_act_after_boot() {
        let mut sup = Supervisor::new(reference_profile());
        sup.boot(&BootAssets::default()).unwrap();
        let g = sup.observe().unwrap().generation;
        let obs = sup
            .act(Action::Tap {
                index: 0,
                generation: g,
            })
            .unwrap();
        assert_eq!(obs.generation, g + 1);
    }

    #[test]
    fn boot_wires_vmm_arg_gen_without_qemu_tell() {
        let mut sup = Supervisor::new(reference_profile());
        assert!(sup.launch_args().is_empty(), "no args before boot");
        sup.boot(&BootAssets::default()).unwrap();
        let joined = sup.launch_args().join(" ");
        // The identity → vmm arg-gen path ran…
        assert!(sup
            .launch_args()
            .windows(2)
            .any(|w| w[0] == "-accel" && w[1] == "hvf"));
        assert!(joined.contains("-dtb"));
        // …and never leaks the emulator tell.
        assert!(!joined.contains("androidboot.qemu"));
    }

    #[test]
    fn act_with_stale_generation_surfaces_control_error_and_stays_ready() {
        let mut sup = Supervisor::new(reference_profile());
        sup.boot(&BootAssets::default()).unwrap();
        let g = sup.observe().unwrap().generation;
        let err = sup
            .act(Action::Tap {
                index: 0,
                generation: g + 7,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            SupervisorError::Control(ControlError::StaleGeneration { .. })
        ));
        // The device is still up; only the action failed.
        assert_eq!(sup.state(), DeviceState::Ready);
        // The failed action did not advance the live generation.
        assert_eq!(sup.observe().unwrap().generation, g);
    }

    /// A test double proving a launch failure drives the device to [`DeviceState::Failed`] and that
    /// a real launcher drops in through [`Supervisor::with_launcher`] with no API change.
    struct FailingLauncher;
    impl Launcher for FailingLauncher {
        fn launch(&self, _p: &DeviceProfile, _a: &BootAssets) -> Result<Launched, SupervisorError> {
            Err(SupervisorError::Vmm(VmmError::BackendUnavailable(
                "test".to_string(),
            )))
        }
    }

    #[test]
    fn failed_launch_transitions_to_failed_and_refuses_observe() {
        let mut sup = Supervisor::with_launcher(reference_profile(), Box::new(FailingLauncher));
        let err = sup.boot(&BootAssets::default()).unwrap_err();
        assert!(matches!(
            err,
            SupervisorError::Vmm(VmmError::BackendUnavailable(_))
        ));
        assert_eq!(sup.state(), DeviceState::Failed);
        match sup.observe() {
            Err(SupervisorError::NotReady(DeviceState::Failed)) => {}
            other => panic!("expected NotReady(Failed), got {other:?}"),
        }
    }
}
