//! Host-side user-mode networking backend (libslirp NAT) for virtio-net.
//!
//! Only compiled under the `net` cargo feature — the default build never links libslirp, so it
//! stays hermetic (Hypervisor.framework + winit only). When present, [`slirp::SlirpNet`] is a
//! zero-privilege user-mode NAT (guest 10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3, built-in DHCP)
//! exactly like `qemu -netdev user` / the Android SDK emulator, bridged to the virtio-net RX/TX
//! virtqueues in `crate::virtio::net`.

pub mod slirp;
pub use slirp::SlirpNet;
