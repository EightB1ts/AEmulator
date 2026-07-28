//! # aemulator-hal
//!
//! The **guest-HAL protocol layer**: host-side state machines that answer the Cuttlefish guest's
//! hardware-abstraction-layer services. Every module here is pure `bytes in → bytes out`. There are
//! no VMM types, no guest-memory access, no virtqueues, no threads and no platform APIs — the
//! embedding VMM owns all of that and calls in through [`HalPort`].
//!
//! ## Why this is its own crate
//!
//! The Cuttlefish guest image ships real HALs that dial out to host daemons. Where a host backend is
//! missing the HAL aborts and init respawns it forever, so earlier work made the guest boot by
//! *binary-patching the image to disable those HALs*. That is the wrong trade twice over: it leaves
//! the device functionally incomplete, and a phone reporting **zero sensors** is a far stronger
//! virtualization tell than any of the build-prop strings `aemulator-identity` works to hide.
//!
//! So these protocols are implemented for real. They live outside the macOS-only
//! `aemulator-hvf` spike because the protocol logic is entirely portable: the Linux/KVM crosvm
//! backend needs the exact same state machines, and keeping them here puts them under the workspace
//! merge gate (`ci/check.sh`: `cargo fmt --check`, `clippy -D warnings`, `cargo test`).
//!
//! ## The transports the guest uses
//!
//! Most Cuttlefish HALs open a dedicated **virtio-console** port (`/dev/hvcN`) in raw termios mode.
//! The port assignment is fixed by the guest's own `ueventd.rc` and is *not* negotiable:
//!
//! | port | consumer | protocol |
//! |------|----------|----------|
//! | hvc2 | logd | line-oriented text |
//! | hvc3/hvc4/hvc10/hvc11 | keymaster / gatekeeper / oemlock / keymint | `cuttlefish::transport` messages |
//! | hvc5 | bluetooth (`vendor.ser.bt-uart`) | HCI H4 |
//! | hvc6/hvc7 | gnss (`/dev/gnss0`, `/dev/gnss1`) | NMEA |
//! | hvc9 | uwb (`ro.vendor.uwb.dev`) | UCI |
//! | hvc12 | nfc (`--virtio-dev-path`) | NCI |
//! | hvc18/hvc19 | sensors control / sensors data | goldfish ASCII sensor protocol |
//!
//! The rest (RIL/modem, lights) dial **AF_VSOCK** to a host port named by a `ro.boot.*` property.
//! [`vsock`] carries the packet layer for those; the services themselves ([`modem`]) are plain
//! byte-stream handlers, transport-agnostic by construction.
//!
//! ## Robustness contract (load-bearing — read before adding a module)
//!
//! In the HVF embedding, [`HalPort::on_message`] runs **synchronously on the vCPU thread that took
//! the MMIO trap**, inside the PCI bus mutex. A panic there therefore poisons the mutex for the
//! *entire* virtual PCI bus, not just the offending port — it takes the whole VM down. Every
//! implementation in this crate must consequently:
//!
//! * treat all input as hostile: bounds-check with `get()`/`try_into()`, never index or slice raw,
//!   never `unwrap()` on anything derived from guest bytes;
//! * never block, sleep, or perform I/O;
//! * always terminate: a malformed length must resync or drop, never loop.
//!
//! Each protocol module carries a `fuzz_lite` test that feeds truncated and pseudo-random input one
//! byte at a time and asserts no panic. Keep that pattern for anything new.

#![forbid(unsafe_code)]

pub mod bluetooth;
pub mod framing;
pub mod gnss;
pub mod modem;
pub mod nfc;
pub mod port;
pub mod sensors;
pub mod vsock;

pub use bluetooth::BtPort;
pub use framing::{Framing, Reassembler};
pub use gnss::{Fix, NmeaSource};
pub use modem::{ModemChannel, ModemState};
pub use nfc::NciPort;
pub use port::HalPort;
pub use sensors::{SensorsControlPort, SensorsState, SENSOR_TABLE};
pub use vsock::{ConnHandle, VsockRegistry, VsockService};
