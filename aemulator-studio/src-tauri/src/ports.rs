//! Host adb-port allocation. Studio owns all per-VM uniqueness — nothing in
//! `aemulator-hvf` auto-allocates. Each running VM forwards guest `:5555` to a
//! distinct host port via `AEMU_HOSTFWD="tcp:<port>:5555"`, so `adb connect
//! 127.0.0.1:<port>` reaches exactly that VM.

use std::collections::HashSet;

/// adb host-forward ports start here and count up, skipping any already handed out.
///
/// **Deliberately above adb's reserved emulator window (5554–5585), not inside it.** The adb server
/// auto-probes the odd ports in that range (5555, 5557, … — `ADB_LOCAL_TRANSPORT_MAX_PORT`) looking
/// for emulators, and treats each as an adb port paired with an *console* port one below it. A
/// forward landing in there gets adopted as a phantom `emulator-<n>` transport instead of the plain
/// `127.0.0.1:<port>` device we told the user to connect to — so the device shows up under the wrong
/// name, `adb connect` can answer "already connected", and console-only verbs get aimed at an adbd
/// that does not speak that protocol. Starting at 15555 puts every VM outside the scanner entirely.
const ADB_PORT_BASE: u16 = 15555;

/// How many candidate ports to try before giving up (keeps a saturated host from looping to 65535).
const MAX_PROBES: u16 = 512;

/// Is this port actually bindable on the loopback right now?
///
/// `in_use` only knows about ports *this* Studio handed out; it cannot see another process (a second
/// Studio, a stale VMM, an unrelated dev server) already holding one. Without this probe that
/// collision surfaces as a hostfwd that silently never binds — the VM boots fine and only `adb
/// connect` fails, which is indistinguishable from the guest simply not being up yet.
///
/// Inherently TOCTOU: the listener is dropped here and the VMM binds moments later. It closes the
/// common case (a port held for the VM's whole lifetime), not a deliberate race.
fn host_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[derive(Default)]
pub struct PortPool {
    in_use: HashSet<u16>,
}

impl PortPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Next free adb host port (`AEMU_HOSTFWD="tcp:<port>:5555"`) — unclaimed by this Studio *and*
    /// bindable on the host right now.
    pub fn alloc_adb(&mut self) -> u16 {
        let mut p = ADB_PORT_BASE;
        for _ in 0..MAX_PROBES {
            if !self.in_use.contains(&p) && host_port_free(p) {
                self.in_use.insert(p);
                return p;
            }
            p = p.saturating_add(1);
        }
        // Every candidate was taken. Hand back the base anyway rather than failing the launch: the
        // VM is still fully usable over the control socket (display, input, screenshots), and the
        // VMM's own hostfwd bind error names the conflict in the log.
        self.in_use.insert(ADB_PORT_BASE);
        ADB_PORT_BASE
    }

    /// Reserve a specific port (used when rehydrating a running set — unused today).
    #[allow(dead_code)]
    pub fn reserve(&mut self, port: u16) {
        self.in_use.insert(port);
    }

    pub fn free(&mut self, port: u16) {
        self.in_use.remove(&port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_are_distinct_and_clear_adbs_emulator_window() {
        let mut pool = PortPool::new();
        let ports: Vec<u16> = (0..4).map(|_| pool.alloc_adb()).collect();
        for (i, p) in ports.iter().enumerate() {
            assert!(
                !(5554..=5585).contains(p),
                "port {p} lands in adb's emulator scan window and would be adopted as a phantom \
                 emulator-* transport"
            );
            assert!(!ports[..i].contains(p), "port {p} handed out twice");
        }
    }

    #[test]
    fn freed_ports_are_released_for_reuse() {
        // Asserts the *bookkeeping*, not "the next alloc returns exactly this port": allocation also
        // consults the live OS, so the concrete port a later call picks is not deterministic (a
        // sibling test's transient bind is enough to move it).
        let mut pool = PortPool::new();
        let first = pool.alloc_adb();
        assert!(pool.in_use.contains(&first));
        pool.free(first);
        assert!(!pool.in_use.contains(&first), "free() must drop the reservation");
    }
}
