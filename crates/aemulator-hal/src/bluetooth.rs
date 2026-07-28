//! Bluetooth: an HCI controller over UART (H4) for the Cuttlefish BT HAL on `/dev/hvc5`.
//!
//! ## What the guest does
//!
//! The image runs
//! `bt_hci /apex/com.google.cf.bt/bin/hw/android.hardware.bluetooth-service.cuttlefish
//! --serial ${vendor.ser.bt-uart}` with `vendor.ser.bt-uart=/dev/hvc5` from `/vendor/build.prop`.
//! With hvc5 wired to a silent drain, `HciProxy::initialize` sent `HCI_Reset` and never heard back, so
//! the guest logged `waitForInitialization: Running in degraded performance mode due to slow hardware`
//! and `GceBluetoothChecker: Bluetooth disabled`. On real Cuttlefish the far end is `rootcanal`, reached
//! through `tcp_connector` — a byte pump whose flag surface (`--fifo_in`, `--fifo_out`, `--data_port`)
//! contains no framing logic, which is what establishes hvc5 as carrying bare H4.
//!
//! Confirmed from the live guest's first write on hvc5: `01 03 0c 00` — H4 type 0x01 (command),
//! opcode 0x0c03 little-endian (OGF 0x03 `HCI_Reset`), parameter length 0.
//!
//! ## H4 framing
//!
//! [`Framing::Bare`]; each packet is self-delimiting via a type byte and a per-type length field:
//!
//! ```text
//! 0x01 COMMAND   [opcode:u16 le][plen:u8 ][params…]
//! 0x02 ACL       [handle:u16 le][len:u16 le][data…]
//! 0x03 SCO       [handle:u16 le][len:u8  ][data…]
//! 0x04 EVENT     [code:u8      ][plen:u8 ][params…]     (controller → host only)
//! 0x05 ISO       [handle:u16 le][len:u16 le][data…]
//! ```
//!
//! ## Scope: a controller with no peers
//!
//! This is a real local controller that simply has nothing in radio range. It answers the whole
//! initialisation handshake, accepts scanning and advertising configuration, and reports that no device
//! was ever found — which is what a phone in an empty room reports. It never fabricates a peer, so
//! there is nothing to connect to and no ACL data path; adding one later means emitting
//! `LE_Advertising_Report` events, not restructuring this.
//!
//! The load-bearing detail for reaching `BLE_ON` is that the LE bits in
//! `Read_Local_Supported_Features` / `Read_LMP_Features` and the LE command bitmap in
//! `Read_Local_Supported_Commands` must actually be set. A controller that resets cleanly but claims no
//! LE support leaves the Android stack refusing to enable.

use crate::framing::Framing;
use crate::port::HalPort;

// H4 packet type indicators.
const H4_COMMAND: u8 = 0x01;
const H4_ACL: u8 = 0x02;
const H4_SCO: u8 = 0x03;
const H4_EVENT: u8 = 0x04;
const H4_ISO: u8 = 0x05;

// Event codes.
const EVT_DISCONNECTION_COMPLETE: u8 = 0x05;
const EVT_COMMAND_COMPLETE: u8 = 0x0e;
const EVT_COMMAND_STATUS: u8 = 0x0f;
const EVT_NUMBER_OF_COMPLETED_PACKETS: u8 = 0x13;

/// `HCI_SUCCESS`.
const STATUS_SUCCESS: u8 = 0x00;
/// `HCI_ERR_UNKNOWN_COMMAND` — the correct refusal for a command we do not implement, and safer than
/// a false success for commands whose return parameters the host would then misparse.
const STATUS_UNKNOWN_COMMAND: u8 = 0x01;

/// Opcodes, as the 16-bit little-endian values that appear on the wire (`OGF << 10 | OCF`).
mod op {
    // OGF 0x01 — Link Control.
    pub const DISCONNECT: u16 = 0x0406;
    pub const READ_REMOTE_VERSION: u16 = 0x041d;
    // OGF 0x03 — Controller & Baseband.
    pub const RESET: u16 = 0x0c03;
    pub const SET_EVENT_MASK: u16 = 0x0c01;
    pub const WRITE_LOCAL_NAME: u16 = 0x0c13;
    pub const READ_LOCAL_NAME: u16 = 0x0c14;
    pub const WRITE_SCAN_ENABLE: u16 = 0x0c1a;
    pub const WRITE_CLASS_OF_DEVICE: u16 = 0x0c24;
    pub const WRITE_SIMPLE_PAIRING_MODE: u16 = 0x0c56;
    pub const SET_EVENT_MASK_PAGE_2: u16 = 0x0c63;
    pub const READ_LE_HOST_SUPPORT: u16 = 0x0c6c;
    pub const WRITE_LE_HOST_SUPPORT: u16 = 0x0c6d;
    pub const WRITE_SECURE_CONNECTIONS_HOST_SUPPORT: u16 = 0x0c7a;
    pub const WRITE_AUTHENTICATED_PAYLOAD_TIMEOUT: u16 = 0x0c7c;
    /// `HCI_Set_Min_Encryption_Key_Size`. Refusing this **aborts the Bluetooth process**:
    /// `controller_impl.cc:437 set_min_encryption_key_size_handler:
    ///  Status UNKNOWN_HCI_COMMAND(0x01)`. The Android stack treats it as mandatory, so a controller
    /// claiming Bluetooth 5.4 must accept it.
    pub const SET_MIN_ENCRYPTION_KEY_SIZE: u16 = 0x0c84;
    pub const READ_BUFFER_SIZE: u16 = 0x1005;
    // The rest of the Controller & Baseband writes the Android stack issues during bring-up. Every one
    // of these was found by the unmodelled-opcode log rather than guessed at, and all return status only.
    pub const WRITE_PAGE_TIMEOUT: u16 = 0x0c18;
    pub const WRITE_PAGE_SCAN_ACTIVITY: u16 = 0x0c1c;
    pub const WRITE_INQUIRY_SCAN_ACTIVITY: u16 = 0x0c1e;
    pub const WRITE_VOICE_SETTING: u16 = 0x0c26;
    pub const WRITE_INQUIRY_SCAN_TYPE: u16 = 0x0c43;
    pub const WRITE_INQUIRY_MODE: u16 = 0x0c45;
    pub const WRITE_PAGE_SCAN_TYPE: u16 = 0x0c47;
    pub const WRITE_EXTENDED_INQUIRY_RESPONSE: u16 = 0x0c52;
    /// `HCI_Read_Default_Erroneous_Data_Reporting` — a *read*, so status + one value byte.
    pub const READ_DEFAULT_ERRONEOUS_DATA_REPORTING: u16 = 0x0c5a;
    /// The matching *write*, status only. Adding the read without the write left one opcode refused.
    pub const WRITE_DEFAULT_ERRONEOUS_DATA_REPORTING: u16 = 0x0c5b;
    // OGF 0x02 — Link Policy.
    pub const WRITE_DEFAULT_LINK_POLICY_SETTINGS: u16 = 0x080f;
    /// `HCI_LE_Set_Data_Length` — returns status + the connection handle it applied to.
    pub const LE_SET_DATA_LENGTH: u16 = 0x202e;
    // OGF 0x04 — Informational.
    pub const READ_LOCAL_VERSION: u16 = 0x1001;
    pub const READ_LOCAL_SUPPORTED_COMMANDS: u16 = 0x1002;
    pub const READ_LOCAL_SUPPORTED_FEATURES: u16 = 0x1003;
    pub const READ_LOCAL_EXTENDED_FEATURES: u16 = 0x1004;
    pub const READ_BD_ADDR: u16 = 0x1009;
    pub const READ_LOCAL_SUPPORTED_CODECS_V1: u16 = 0x100b;
    pub const READ_LOCAL_SUPPORTED_CODECS_V2: u16 = 0x100e;
    // OGF 0x08 — LE Controller.
    pub const LE_SET_EVENT_MASK: u16 = 0x2001;
    pub const LE_READ_BUFFER_SIZE_V1: u16 = 0x2002;
    pub const LE_READ_LOCAL_SUPPORTED_FEATURES: u16 = 0x2003;
    pub const LE_SET_RANDOM_ADDRESS: u16 = 0x2005;
    pub const LE_SET_ADVERTISING_PARAMETERS: u16 = 0x2006;
    pub const LE_SET_ADVERTISING_DATA: u16 = 0x2008;
    pub const LE_SET_SCAN_RESPONSE_DATA: u16 = 0x2009;
    pub const LE_SET_ADVERTISING_ENABLE: u16 = 0x200a;
    pub const LE_SET_SCAN_PARAMETERS: u16 = 0x200b;
    pub const LE_SET_SCAN_ENABLE: u16 = 0x200c;
    pub const LE_CREATE_CONNECTION: u16 = 0x200d;
    pub const LE_CREATE_CONNECTION_CANCEL: u16 = 0x200e;
    pub const LE_READ_FILTER_ACCEPT_LIST_SIZE: u16 = 0x200f;
    pub const LE_CLEAR_FILTER_ACCEPT_LIST: u16 = 0x2010;
    pub const LE_RAND: u16 = 0x2018;
    pub const LE_READ_SUPPORTED_STATES: u16 = 0x201c;
    pub const LE_READ_MAX_DATA_LENGTH: u16 = 0x202f;
    pub const LE_READ_SUGGESTED_DEFAULT_DATA_LENGTH: u16 = 0x2023;
    pub const LE_READ_RESOLVING_LIST_SIZE: u16 = 0x202a;
    pub const LE_READ_PERIODIC_ADVERTISER_LIST_SIZE: u16 = 0x204a;
    pub const LE_READ_BUFFER_SIZE_V2: u16 = 0x2060;
    pub const LE_READ_TRANSMIT_POWER: u16 = 0x204b;
    pub const LE_WRITE_SUGGESTED_DEFAULT_DATA_LENGTH: u16 = 0x2024;
    pub const LE_SET_DEFAULT_PHY: u16 = 0x2031;
    pub const LE_READ_NUMBER_OF_SUPPORTED_ADV_SETS: u16 = 0x203b;
    pub const LE_READ_MAX_ADV_DATA_LENGTH: u16 = 0x203a;
    pub const LE_SET_EXTENDED_SCAN_PARAMETERS: u16 = 0x2041;
    pub const LE_SET_EXTENDED_SCAN_ENABLE: u16 = 0x2042;
    pub const LE_SET_EXTENDED_ADVERTISING_ENABLE: u16 = 0x2039;
    pub const LE_CLEAR_ADVERTISING_SETS: u16 = 0x203d;
    pub const LE_SET_HOST_FEATURE: u16 = 0x2074;
}

/// The controller's BD_ADDR, little-endian on the wire.
///
/// A locally-administered address (bit 1 of the first octet set, per IEEE 802) in the same spirit as
/// the virtio-net MAC: unmistakably not a real vendor OUI, and stable across boots so pairing records
/// stay valid.
const BD_ADDR: [u8; 6] = [0x66, 0x55, 0x44, 0x33, 0x22, 0x02];

/// Local name reported by `Read_Local_Name`, NUL-padded to 248 bytes on the wire.
const LOCAL_NAME: &str = "Bluetooth";

/// Largest H4 payload we will buffer before deciding the stream is desynchronised.
const MAX_H4_BODY: usize = 4096;

/// An HCI controller with no peers in radio range.
pub struct BtPort {
    buf: Vec<u8>,
    /// Number of commands answered — surfaced for diagnostics and tests.
    commands: u64,
    /// Set once the host has enabled LE host support, purely so `Read_LE_Host_Support` can echo it.
    le_host_support: bool,
    /// Local name as last written by the host.
    local_name: String,
    /// Deterministic counter feeding `LE_Rand`. A real controller returns entropy; we must return
    /// *something* eight bytes long, and a counter keeps tests reproducible.
    rand_counter: u64,
    /// True once the stream has desynchronised and been dropped.
    desynced: bool,
    /// Opcodes seen that we do not model, recorded once each so the log names every gap exactly once.
    unmodelled: Vec<u16>,
}

impl Default for BtPort {
    fn default() -> Self {
        Self::new()
    }
}

impl BtPort {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            commands: 0,
            le_host_support: false,
            local_name: LOCAL_NAME.to_string(),
            rand_counter: 0x0123_4567_89ab_cdef,
            desynced: false,
            unmodelled: Vec::new(),
        }
    }

    pub fn commands(&self) -> u64 {
        self.commands
    }

    pub fn desynced(&self) -> bool {
        self.desynced
    }

    /// Opcodes this controller was asked for but does not model. Empty is the goal.
    pub fn unmodelled(&self) -> &[u16] {
        &self.unmodelled
    }

    /// Build an `HCI_Command_Complete` event: `num_hci_command_packets` = 1 (one credit returned),
    /// the opcode being completed, then the command's return parameters.
    fn command_complete(opcode: u16, ret: &[u8]) -> Vec<u8> {
        let mut params = Vec::with_capacity(3 + ret.len());
        params.push(0x01); // command credits freed
        params.extend_from_slice(&opcode.to_le_bytes());
        params.extend_from_slice(ret);
        Self::event(EVT_COMMAND_COMPLETE, &params)
    }

    /// Build an `HCI_Command_Status` event — the correct reply for commands that complete
    /// asynchronously (`LE_Create_Connection`), where a Command_Complete would confuse the host.
    fn command_status(status: u8, opcode: u16) -> Vec<u8> {
        let mut params = Vec::with_capacity(4);
        params.push(status);
        params.push(0x01); // command credits freed
        params.extend_from_slice(&opcode.to_le_bytes());
        Self::event(EVT_COMMAND_STATUS, &params)
    }

    /// Wrap event parameters in an H4 event packet. Parameters are truncated at 255 bytes because the
    /// length field is a single byte.
    fn event(code: u8, params: &[u8]) -> Vec<u8> {
        let n = params.len().min(255);
        let mut v = Vec::with_capacity(3 + n);
        v.push(H4_EVENT);
        v.push(code);
        v.push(n as u8);
        v.extend_from_slice(&params[..n]);
        v
    }

    /// Answer one HCI command.
    fn handle_command(&mut self, opcode: u16, params: &[u8]) -> Vec<Vec<u8>> {
        self.commands += 1;
        let ok = |ret: &[u8]| vec![Self::command_complete(opcode, ret)];
        match opcode {
            op::RESET => ok(&[STATUS_SUCCESS]),

            op::READ_LOCAL_VERSION => {
                // status, hci_version, hci_revision, lmp_version, manufacturer, lmp_subversion.
                // Version 0x0d = Bluetooth 5.4; the Android stack gates LE Extended Advertising and
                // several other features on this being recent enough.
                ok(&[
                    STATUS_SUCCESS,
                    0x0d,
                    0x00,
                    0x00, // hci revision
                    0x0d, // lmp version
                    0x0f,
                    0x00, // manufacturer (0x000f, a placeholder assignee)
                    0x00,
                    0x00, // lmp subversion
                ])
            }

            op::READ_LOCAL_SUPPORTED_COMMANDS => {
                // 64-byte command bitmap. Rather than enumerate every octet, assert broad support:
                // the Android stack uses this only to decide whether it *may* issue a command, and a
                // command we did not model still gets a well-formed UNKNOWN_COMMAND reply below.
                let mut ret = vec![0xffu8; 65];
                ret[0] = STATUS_SUCCESS;
                ok(&ret)
            }

            op::READ_LOCAL_SUPPORTED_FEATURES => {
                // 8-byte LMP feature mask. Byte 4 bit 6 = BR/EDR Not Supported is left CLEAR and byte
                // 4 bit 6 (0x40) LE Supported is SET — this pair is what tells the host the controller
                // is dual-mode. Getting this wrong is the classic reason a stack never reaches BLE_ON.
                ok(&[
                    STATUS_SUCCESS,
                    0xff,
                    0xff,
                    0x8f,
                    0xfe,
                    0xdb,
                    0xff,
                    0x5b,
                    0x87,
                ])
            }

            op::READ_LOCAL_EXTENDED_FEATURES => {
                // status, page_number, maximum_page_number, extended_lmp_features[8] — 11 return bytes.
                //
                // This one is load-bearing and was learned the hard way: the Android stack parses the
                // reply into a typed `ReadLocalExtendedFeaturesCompleteView` and *aborts the whole
                // Bluetooth process* if the length does not match —
                // `controller_impl.cc:359 read_local_extended_features_complete_handler:
                //  Complete view is invalid`. A bare status byte (the generic UNKNOWN_COMMAND refusal)
                // is therefore fatal here, not merely unhelpful. Any command the stack has a typed view
                // for must get its full return structure, which is why the fallback below logs.
                let page = params.first().copied().unwrap_or(0);
                let mut ret = vec![STATUS_SUCCESS, page, 0x02];
                // Page 0 repeats the standard LMP features; pages 1-2 are the extended pages. Page 1
                // bit 0 = Secure Simple Pairing (Host Support), bit 1 = LE Supported (Host).
                match page {
                    0 => ret.extend_from_slice(&[0xff, 0xff, 0x8f, 0xfe, 0xdb, 0xff, 0x5b, 0x87]),
                    1 => ret.extend_from_slice(&[0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
                    _ => ret.extend_from_slice(&[0x00; 8]),
                }
                ok(&ret)
            }

            op::READ_BD_ADDR => {
                let mut ret = vec![STATUS_SUCCESS];
                ret.extend_from_slice(&BD_ADDR);
                ok(&ret)
            }

            op::LE_READ_BUFFER_SIZE_V2 => {
                // status, le_acl_len:u16, le_acl_packets:u8, iso_len:u16, iso_packets:u8.
                ok(&[STATUS_SUCCESS, 0xfb, 0x00, 0x08, 0xfb, 0x00, 0x04])
            }

            op::LE_READ_RESOLVING_LIST_SIZE => ok(&[STATUS_SUCCESS, 0x08]),

            op::LE_READ_PERIODIC_ADVERTISER_LIST_SIZE => ok(&[STATUS_SUCCESS, 0x08]),

            op::LE_READ_TRANSMIT_POWER => {
                // status, min_tx_power:i8, max_tx_power:i8 (dBm).
                ok(&[STATUS_SUCCESS, 0xf4, 0x09]) // -12 dBm .. +9 dBm
            }

            op::READ_LOCAL_SUPPORTED_CODECS_V1 => {
                // status, num_standard_codecs=0, num_vendor_codecs=0.
                ok(&[STATUS_SUCCESS, 0x00, 0x00])
            }

            op::READ_DEFAULT_ERRONEOUS_DATA_REPORTING => {
                // status + erroneous_data_reporting (0 = disabled).
                ok(&[STATUS_SUCCESS, 0x00])
            }

            op::LE_SET_DATA_LENGTH => {
                // status + the connection handle from the request, echoed back.
                let handle = u16::from_le_bytes([
                    params.first().copied().unwrap_or(0),
                    params.get(1).copied().unwrap_or(0),
                ]);
                let mut ret = vec![STATUS_SUCCESS];
                ret.extend_from_slice(&handle.to_le_bytes());
                ok(&ret)
            }

            op::READ_BUFFER_SIZE => {
                // acl_len:u16, sco_len:u8, acl_packets:u16, sco_packets:u16.
                ok(&[
                    STATUS_SUCCESS,
                    0xfb,
                    0x00, // ACL data length 251
                    0x3c, // SCO length 60
                    0x08,
                    0x00, // 8 ACL packets
                    0x08,
                    0x00, // 8 SCO packets
                ])
            }

            op::LE_READ_BUFFER_SIZE_V1 => {
                // le_acl_len:u16, le_acl_packets:u8.
                ok(&[STATUS_SUCCESS, 0xfb, 0x00, 0x08])
            }

            op::LE_READ_LOCAL_SUPPORTED_FEATURES => {
                // 8-byte LE feature mask: LE Encryption, Extended Advertising, 2M PHY, Data Length
                // Extension, Privacy — the set the modern Android stack expects to find.
                ok(&[
                    STATUS_SUCCESS,
                    0x7f,
                    0x1f,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                ])
            }

            op::LE_READ_SUPPORTED_STATES => {
                // 8-byte state/role-combination bitmap; all-ones claims every combination.
                ok(&[
                    STATUS_SUCCESS,
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    0x1f,
                ])
            }

            op::LE_READ_FILTER_ACCEPT_LIST_SIZE => ok(&[STATUS_SUCCESS, 0x08]),

            op::LE_READ_NUMBER_OF_SUPPORTED_ADV_SETS => ok(&[STATUS_SUCCESS, 0x08]),

            op::LE_READ_MAX_ADV_DATA_LENGTH => {
                // 1650 bytes, the usual extended-advertising maximum.
                ok(&[STATUS_SUCCESS, 0x72, 0x06])
            }

            op::LE_READ_MAX_DATA_LENGTH => {
                // max_tx_octets, max_tx_time, max_rx_octets, max_rx_time.
                ok(&[
                    STATUS_SUCCESS,
                    0xfb,
                    0x00,
                    0x48,
                    0x08,
                    0xfb,
                    0x00,
                    0x48,
                    0x08,
                ])
            }

            op::LE_READ_SUGGESTED_DEFAULT_DATA_LENGTH => {
                ok(&[STATUS_SUCCESS, 0x1b, 0x00, 0x48, 0x00])
            }

            op::LE_RAND => {
                // status + 8 random octets.
                self.rand_counter = self
                    .rand_counter
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let mut ret = vec![STATUS_SUCCESS];
                ret.extend_from_slice(&self.rand_counter.to_le_bytes());
                ok(&ret)
            }

            op::READ_LOCAL_NAME => {
                // status + exactly 248 bytes, NUL-padded.
                let mut ret = vec![STATUS_SUCCESS];
                let mut name = self.local_name.as_bytes().to_vec();
                name.resize(248, 0);
                ret.extend_from_slice(&name);
                ok(&ret)
            }

            op::WRITE_LOCAL_NAME => {
                if let Some(end) = params.iter().position(|&b| b == 0) {
                    if let Ok(s) = std::str::from_utf8(&params[..end]) {
                        self.local_name = s.to_string();
                    }
                }
                ok(&[STATUS_SUCCESS])
            }

            op::WRITE_LE_HOST_SUPPORT => {
                self.le_host_support = params.first().copied().unwrap_or(0) == 1;
                ok(&[STATUS_SUCCESS])
            }

            op::READ_LE_HOST_SUPPORT => ok(&[STATUS_SUCCESS, u8::from(self.le_host_support), 0x00]),

            op::READ_LOCAL_SUPPORTED_CODECS_V2 => {
                // status, num_standard_codecs=0, num_vendor_codecs=0.
                ok(&[STATUS_SUCCESS, 0x00, 0x00])
            }

            // Commands that only need an acknowledgement. Listed explicitly rather than swept into the
            // fallback so it stays visible which parts of initialisation are deliberately no-ops.
            op::SET_EVENT_MASK
            | op::SET_EVENT_MASK_PAGE_2
            | op::LE_SET_EVENT_MASK
            | op::WRITE_SCAN_ENABLE
            | op::WRITE_CLASS_OF_DEVICE
            | op::WRITE_SIMPLE_PAIRING_MODE
            | op::WRITE_SECURE_CONNECTIONS_HOST_SUPPORT
            | op::WRITE_AUTHENTICATED_PAYLOAD_TIMEOUT
            | op::LE_SET_RANDOM_ADDRESS
            | op::LE_SET_ADVERTISING_PARAMETERS
            | op::LE_SET_ADVERTISING_DATA
            | op::LE_SET_SCAN_RESPONSE_DATA
            | op::LE_SET_ADVERTISING_ENABLE
            | op::LE_SET_EXTENDED_ADVERTISING_ENABLE
            | op::LE_SET_SCAN_PARAMETERS
            | op::LE_SET_EXTENDED_SCAN_PARAMETERS
            | op::LE_CLEAR_FILTER_ACCEPT_LIST
            | op::LE_CLEAR_ADVERTISING_SETS
            | op::LE_WRITE_SUGGESTED_DEFAULT_DATA_LENGTH
            | op::LE_SET_DEFAULT_PHY
            | op::LE_SET_HOST_FEATURE
            // Mandatory in the Android stack's eyes: a non-success status here aborts the process
            // outright rather than degrading, so it must be accepted, not refused.
            | op::SET_MIN_ENCRYPTION_KEY_SIZE
            // The remaining bring-up writes, all enumerated by the unmodelled-opcode log.
            | op::WRITE_PAGE_TIMEOUT
            | op::WRITE_PAGE_SCAN_ACTIVITY
            | op::WRITE_INQUIRY_SCAN_ACTIVITY
            | op::WRITE_VOICE_SETTING
            | op::WRITE_INQUIRY_SCAN_TYPE
            | op::WRITE_INQUIRY_MODE
            | op::WRITE_PAGE_SCAN_TYPE
            | op::WRITE_EXTENDED_INQUIRY_RESPONSE
            | op::WRITE_DEFAULT_LINK_POLICY_SETTINGS
            | op::WRITE_DEFAULT_ERRONEOUS_DATA_REPORTING => ok(&[STATUS_SUCCESS]),

            // Scanning succeeds and then finds nothing: no LE_Advertising_Report ever follows. That is
            // a phone in an empty room, and it is why there is no peer to connect to.
            op::LE_SET_SCAN_ENABLE | op::LE_SET_EXTENDED_SCAN_ENABLE => ok(&[STATUS_SUCCESS]),

            // Connecting to a peer that does not exist. Command_Status (not Complete) is the correct
            // reply shape; the connection then simply never completes, and the host's own timeout
            // handles it — exactly as with a device that walked out of range.
            op::LE_CREATE_CONNECTION => vec![Self::command_status(STATUS_SUCCESS, opcode)],
            op::LE_CREATE_CONNECTION_CANCEL => ok(&[STATUS_SUCCESS]),

            op::DISCONNECT => {
                // Command_Status now, then Disconnection_Complete for the handle the host named.
                let handle = u16::from_le_bytes([
                    params.first().copied().unwrap_or(0),
                    params.get(1).copied().unwrap_or(0),
                ]);
                let mut ev = vec![STATUS_SUCCESS];
                ev.extend_from_slice(&handle.to_le_bytes());
                ev.push(0x16); // reason: connection terminated by local host
                vec![
                    Self::command_status(STATUS_SUCCESS, opcode),
                    Self::event(EVT_DISCONNECTION_COMPLETE, &ev),
                ]
            }

            op::READ_REMOTE_VERSION => vec![Self::command_status(STATUS_SUCCESS, opcode)],

            // Anything else: a well-formed refusal. UNKNOWN_COMMAND is a state every HCI host already
            // handles, because controllers legitimately differ in what they implement.
            //
            // But note the sharp edge, which cost a Bluetooth-process abort to find: for any command the
            // Android stack has a *typed* CompleteView and a handler for, a 1-byte status reply fails
            // its length check and aborts the process outright (see READ_LOCAL_EXTENDED_FEATURES). A
            // refusal is only safe for commands the stack treats generically. So record every opcode
            // that lands here — one boot then enumerates exactly what still needs a real return
            // structure, instead of guessing at the HCI surface.
            _ => {
                let ogf = opcode >> 10;
                // OGF 0x3f is the vendor-specific group. Refusing a vendor command is *correct* — real
                // controllers differ in what they implement and every host handles the refusal — so this
                // is not a gap to be closed, and it is logged distinctly so it does not read as one.
                let vendor = ogf == 0x3f;
                if self.unmodelled.len() < 64 && !self.unmodelled.contains(&opcode) {
                    if vendor {
                        eprintln!(
                            "[hal:bluetooth] vendor-specific opcode {opcode:#06x} refused \
                             (expected: we implement no vendor extensions)"
                        );
                    } else {
                        self.unmodelled.push(opcode);
                        eprintln!(
                            "[hal:bluetooth] unmodelled opcode {opcode:#06x} (OGF {ogf:#x} OCF {:#x}) \
                             — refused with UNKNOWN_COMMAND. If the stack parses this into a typed view \
                             it will abort; give it a full return structure.",
                            opcode & 0x3ff
                        );
                    }
                }
                ok(&[STATUS_UNKNOWN_COMMAND])
            }
        }
    }
}

impl HalPort for BtPort {
    fn label(&self) -> &'static str {
        "bluetooth(hvc5)"
    }

    fn framing(&self) -> Framing {
        Framing::Bare
    }

    fn on_message(&mut self, msg: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(msg);
        let mut out = Vec::new();
        while let Some(&kind) = self.buf.first() {
            // Header length per packet type, and where the body length lives.
            let (hdr, body) = match kind {
                H4_COMMAND => {
                    // type + opcode:u16 + plen:u8
                    if self.buf.len() < 4 {
                        break;
                    }
                    (4usize, self.buf[3] as usize)
                }
                H4_ACL | H4_ISO => {
                    // type + handle:u16 + len:u16
                    if self.buf.len() < 5 {
                        break;
                    }
                    (
                        5usize,
                        u16::from_le_bytes([self.buf[3], self.buf[4]]) as usize,
                    )
                }
                H4_SCO => {
                    // type + handle:u16 + len:u8
                    if self.buf.len() < 4 {
                        break;
                    }
                    (4usize, self.buf[3] as usize)
                }
                _ => {
                    // An event, or garbage: the host must never send us either. We cannot know the
                    // length of something we do not recognise, so resynchronising means dropping the
                    // stream — guessing a length would misframe everything after it.
                    self.desynced = true;
                    self.buf.clear();
                    break;
                }
            };
            if body > MAX_H4_BODY {
                self.desynced = true;
                self.buf.clear();
                break;
            }
            if self.buf.len() < hdr + body {
                break; // wait for the rest
            }
            let pkt: Vec<u8> = self.buf.drain(0..hdr + body).collect();
            match kind {
                H4_COMMAND => {
                    let opcode = u16::from_le_bytes([pkt[1], pkt[2]]);
                    out.extend(self.handle_command(opcode, &pkt[hdr..]));
                }
                H4_ACL | H4_ISO | H4_SCO => {
                    // No connection exists, so any data packet is spurious. Returning the credit keeps
                    // the host's flow control from stalling; silently dropping it would eventually
                    // exhaust its buffer allowance.
                    let handle = u16::from_le_bytes([pkt[1], pkt[2]]);
                    let mut params = vec![0x01]; // one handle follows
                    params.extend_from_slice(&handle.to_le_bytes());
                    params.extend_from_slice(&1u16.to_le_bytes()); // one packet completed
                    out.push(Self::event(EVT_NUMBER_OF_COMPLETED_PACKETS, &params));
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(opcode: u16, params: &[u8]) -> Vec<u8> {
        let mut v = vec![H4_COMMAND];
        v.extend_from_slice(&opcode.to_le_bytes());
        v.push(params.len() as u8);
        v.extend_from_slice(params);
        v
    }

    /// Decode a Command_Complete event into (opcode, return-parameters).
    fn parse_complete(pkt: &[u8]) -> (u16, Vec<u8>) {
        assert_eq!(pkt[0], H4_EVENT, "not an event packet");
        assert_eq!(pkt[1], EVT_COMMAND_COMPLETE, "not Command_Complete");
        let plen = pkt[2] as usize;
        assert_eq!(
            pkt.len(),
            3 + plen,
            "declared length disagrees with the packet"
        );
        assert_eq!(pkt[3], 0x01, "must return one command credit");
        let opcode = u16::from_le_bytes([pkt[4], pkt[5]]);
        (opcode, pkt[6..].to_vec())
    }

    /// The exact bytes the live guest sent first, captured by the VMM port probe on hvc5.
    #[test]
    fn answers_the_real_first_command_seen_from_the_guest() {
        let mut p = BtPort::new();
        let out = p.on_message(&[0x01, 0x03, 0x0c, 0x00]);
        assert_eq!(out.len(), 1, "HCI_Reset must be answered");
        let (opcode, ret) = parse_complete(&out[0]);
        assert_eq!(opcode, op::RESET);
        assert_eq!(ret, vec![STATUS_SUCCESS]);
    }

    /// The controller must claim LE support, or the Android stack never reaches BLE_ON no matter how
    /// cleanly everything else answers.
    #[test]
    fn advertises_le_support_in_both_feature_masks() {
        let mut p = BtPort::new();
        let out = p.on_message(&cmd(op::READ_LOCAL_SUPPORTED_FEATURES, &[]));
        let (_, ret) = parse_complete(&out[0]);
        assert_eq!(ret.len(), 9, "status + 8 LMP feature octets");
        // LMP page 0, byte 4 (ret[5] after the status byte), bit 6 = LE Supported (Controller).
        assert!(
            ret[5] & 0x40 != 0,
            "LE Supported bit is clear in the LMP mask"
        );

        let out = p.on_message(&cmd(op::LE_READ_LOCAL_SUPPORTED_FEATURES, &[]));
        let (_, ret) = parse_complete(&out[0]);
        assert_eq!(ret.len(), 9, "status + 8 LE feature octets");
        assert!(ret[1] & 0x01 != 0, "LE Encryption must be supported");
        // Byte 1 bit 4 (0x10) = LE Extended Advertising.
        assert!(
            ret[2] & 0x10 != 0,
            "LE Extended Advertising must be supported"
        );
    }

    #[test]
    fn buffer_sizes_are_well_formed() {
        let mut p = BtPort::new();
        let (_, ret) = parse_complete(&p.on_message(&cmd(op::READ_BUFFER_SIZE, &[]))[0]);
        assert_eq!(ret.len(), 8, "status + acl_len + sco_len + 2 counts");
        assert!(
            u16::from_le_bytes([ret[1], ret[2]]) >= 27,
            "ACL length too small"
        );

        let (_, ret) = parse_complete(&p.on_message(&cmd(op::LE_READ_BUFFER_SIZE_V1, &[]))[0]);
        assert_eq!(ret.len(), 4);
        assert!(ret[3] > 0, "must advertise at least one LE ACL packet");
    }

    #[test]
    fn read_bd_addr_returns_a_locally_administered_address() {
        let mut p = BtPort::new();
        let (_, ret) = parse_complete(&p.on_message(&cmd(op::READ_BD_ADDR, &[]))[0]);
        assert_eq!(ret.len(), 7, "status + 6 address octets");
        // On the wire the address is little-endian, so the most-significant octet is last.
        assert!(
            ret[6] & 0x02 != 0,
            "should be locally administered, not a real vendor OUI"
        );
        assert!(ret[6] & 0x01 == 0, "must be a unicast address");
    }

    #[test]
    fn read_local_name_is_exactly_248_bytes_and_round_trips_a_write() {
        let mut p = BtPort::new();
        let mut name = b"AEmuPhone".to_vec();
        name.resize(248, 0);
        let out = p.on_message(&cmd(op::WRITE_LOCAL_NAME, &name));
        assert_eq!(parse_complete(&out[0]).1, vec![STATUS_SUCCESS]);
        let (_, ret) = parse_complete(&p.on_message(&cmd(op::READ_LOCAL_NAME, &[]))[0]);
        assert_eq!(ret.len(), 249, "status + exactly 248 name bytes");
        assert!(ret[1..].starts_with(b"AEmuPhone"));
    }

    #[test]
    fn le_host_support_round_trips() {
        let mut p = BtPort::new();
        p.on_message(&cmd(op::WRITE_LE_HOST_SUPPORT, &[0x01, 0x00]));
        let (_, ret) = parse_complete(&p.on_message(&cmd(op::READ_LE_HOST_SUPPORT, &[]))[0]);
        assert_eq!(ret, vec![STATUS_SUCCESS, 0x01, 0x00]);
    }

    /// Scanning must succeed and then report nothing — no advertising reports at all.
    #[test]
    fn scanning_succeeds_but_reports_no_devices() {
        let mut p = BtPort::new();
        let out = p.on_message(&cmd(
            op::LE_SET_EXTENDED_SCAN_ENABLE,
            &[0x01, 0x00, 0x00, 0x00],
        ));
        assert_eq!(out.len(), 1, "only the Command_Complete, no reports");
        assert_eq!(parse_complete(&out[0]).1, vec![STATUS_SUCCESS]);
    }

    /// An async command must get Command_Status, not Command_Complete, or the host mis-sequences.
    #[test]
    fn le_create_connection_gets_command_status() {
        let mut p = BtPort::new();
        let out = p.on_message(&cmd(op::LE_CREATE_CONNECTION, &[0u8; 25]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0], H4_EVENT);
        assert_eq!(out[0][1], EVT_COMMAND_STATUS, "must be Command_Status");
        assert_eq!(out[0][3], STATUS_SUCCESS);
        assert_eq!(
            u16::from_le_bytes([out[0][5], out[0][6]]),
            op::LE_CREATE_CONNECTION
        );
    }

    /// An unmodelled command must be refused, not falsely acknowledged: the host parses return
    /// parameters per opcode, so a bare "success" for an unknown command feeds it garbage.
    #[test]
    fn unmodelled_command_is_refused_with_unknown_command() {
        let mut p = BtPort::new();
        let out = p.on_message(&cmd(0x0c99, &[]));
        let (opcode, ret) = parse_complete(&out[0]);
        assert_eq!(
            opcode, 0x0c99,
            "must echo the opcode so the host can match it"
        );
        assert_eq!(ret, vec![STATUS_UNKNOWN_COMMAND]);
    }

    #[test]
    fn acl_data_returns_its_flow_control_credit() {
        let mut p = BtPort::new();
        let mut acl = vec![H4_ACL, 0x40, 0x00];
        acl.extend_from_slice(&3u16.to_le_bytes());
        acl.extend_from_slice(&[1, 2, 3]);
        let out = p.on_message(&acl);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][1], EVT_NUMBER_OF_COMPLETED_PACKETS);
        assert_eq!(u16::from_le_bytes([out[0][4], out[0][5]]), 0x0040);
    }

    #[test]
    fn reassembles_commands_split_across_byte_at_a_time_writes() {
        let mut p = BtPort::new();
        let wire = cmd(op::READ_BD_ADDR, &[]);
        for (i, b) in wire.iter().enumerate() {
            let got = p.on_message(&[*b]);
            if i + 1 == wire.len() {
                assert_eq!(got.len(), 1, "answers only once the command is whole");
            } else {
                assert!(got.is_empty(), "answered early at byte {i}");
            }
        }
    }

    #[test]
    fn several_commands_in_one_write_are_all_answered() {
        let mut p = BtPort::new();
        let mut wire = cmd(op::RESET, &[]);
        wire.extend(cmd(op::READ_BD_ADDR, &[]));
        wire.extend(cmd(op::READ_BUFFER_SIZE, &[]));
        let out = p.on_message(&wire);
        assert_eq!(out.len(), 3);
        assert_eq!(parse_complete(&out[0]).0, op::RESET);
        assert_eq!(parse_complete(&out[1]).0, op::READ_BD_ADDR);
        assert_eq!(parse_complete(&out[2]).0, op::READ_BUFFER_SIZE);
    }

    /// An unknown H4 type has no discoverable length, so the only safe action is to drop the stream
    /// rather than guess and misframe everything after it.
    #[test]
    fn unknown_h4_type_resyncs_instead_of_guessing() {
        let mut p = BtPort::new();
        assert!(p.on_message(&[0x77, 0x01, 0x02]).is_empty());
        assert!(p.desynced());
    }

    /// The full initialisation sequence a real Android stack issues must complete without a single
    /// UNKNOWN_COMMAND, since any refusal there is what leaves the adapter stuck off.
    #[test]
    fn the_whole_init_handshake_is_answered_successfully() {
        let mut p = BtPort::new();
        let sequence = [
            op::RESET,
            op::READ_LOCAL_VERSION,
            op::READ_LOCAL_SUPPORTED_COMMANDS,
            op::READ_LOCAL_SUPPORTED_FEATURES,
            op::READ_BD_ADDR,
            op::READ_BUFFER_SIZE,
            op::SET_EVENT_MASK,
            op::WRITE_LE_HOST_SUPPORT,
            op::LE_SET_EVENT_MASK,
            op::LE_READ_BUFFER_SIZE_V1,
            op::LE_READ_LOCAL_SUPPORTED_FEATURES,
            op::LE_READ_SUPPORTED_STATES,
            op::LE_READ_FILTER_ACCEPT_LIST_SIZE,
            op::LE_READ_MAX_DATA_LENGTH,
            op::LE_READ_SUGGESTED_DEFAULT_DATA_LENGTH,
            op::LE_READ_NUMBER_OF_SUPPORTED_ADV_SETS,
            op::LE_READ_MAX_ADV_DATA_LENGTH,
            op::WRITE_SIMPLE_PAIRING_MODE,
            op::WRITE_SECURE_CONNECTIONS_HOST_SUPPORT,
            op::READ_LOCAL_SUPPORTED_CODECS_V2,
        ];
        for opcode in sequence {
            let out = p.on_message(&cmd(opcode, &[0x01, 0x00]));
            assert_eq!(
                out.len(),
                1,
                "opcode {opcode:#06x} produced {} events",
                out.len()
            );
            let (got, ret) = parse_complete(&out[0]);
            assert_eq!(got, opcode);
            assert_eq!(
                ret.first().copied(),
                Some(STATUS_SUCCESS),
                "opcode {opcode:#06x} was refused — this is what keeps the adapter off"
            );
        }
        assert_eq!(p.commands(), sequence.len() as u64);
    }

    /// Regression test for a real Bluetooth-process abort.
    ///
    /// `Read_Local_Extended_Features` used to fall through to the 1-byte UNKNOWN_COMMAND refusal. The
    /// Android stack parses this reply into a typed `ReadLocalExtendedFeaturesCompleteView`, so the
    /// short reply failed its length check and killed the process:
    /// `controller_impl.cc:359 read_local_extended_features_complete_handler: Complete view is invalid`.
    /// The reply must be exactly status + page + max_page + 8 feature octets.
    #[test]
    fn read_local_extended_features_returns_the_full_typed_structure() {
        let mut p = BtPort::new();
        for page in 0u8..=2 {
            let out = p.on_message(&cmd(op::READ_LOCAL_EXTENDED_FEATURES, &[page]));
            let (opcode, ret) = parse_complete(&out[0]);
            assert_eq!(opcode, op::READ_LOCAL_EXTENDED_FEATURES);
            assert_eq!(
                ret.len(),
                11,
                "page {page}: must be status + page + max_page + 8 features, or the stack aborts"
            );
            assert_eq!(ret[0], STATUS_SUCCESS);
            assert_eq!(ret[1], page, "must echo the requested page");
            assert!(
                ret[2] >= page,
                "maximum_page_number must cover the page asked for"
            );
        }
        assert!(
            p.unmodelled().is_empty(),
            "this command must be modelled, not refused: {:?}",
            p.unmodelled()
        );
    }

    /// Every command whose reply the stack parses into a typed view must have a return structure longer
    /// than a bare status byte. This guards the whole class of bug above, not just the one instance.
    #[test]
    fn typed_read_commands_return_more_than_a_bare_status() {
        let typed_reads = [
            (op::READ_LOCAL_VERSION, 9usize),
            (op::READ_LOCAL_SUPPORTED_FEATURES, 9),
            (op::READ_LOCAL_EXTENDED_FEATURES, 11),
            (op::READ_BD_ADDR, 7),
            (op::READ_BUFFER_SIZE, 8),
            (op::LE_READ_BUFFER_SIZE_V1, 4),
            (op::LE_READ_BUFFER_SIZE_V2, 7),
            (op::LE_READ_LOCAL_SUPPORTED_FEATURES, 9),
            (op::LE_READ_SUPPORTED_STATES, 9),
            (op::LE_READ_FILTER_ACCEPT_LIST_SIZE, 2),
            (op::LE_READ_RESOLVING_LIST_SIZE, 2),
            (op::LE_READ_PERIODIC_ADVERTISER_LIST_SIZE, 2),
            (op::LE_READ_NUMBER_OF_SUPPORTED_ADV_SETS, 2),
            (op::LE_READ_MAX_ADV_DATA_LENGTH, 3),
            (op::LE_READ_MAX_DATA_LENGTH, 9),
            (op::LE_READ_SUGGESTED_DEFAULT_DATA_LENGTH, 5),
            (op::LE_READ_TRANSMIT_POWER, 3),
            (op::READ_LOCAL_SUPPORTED_CODECS_V1, 3),
            (op::READ_LOCAL_SUPPORTED_CODECS_V2, 3),
            (op::LE_RAND, 9),
        ];
        let mut p = BtPort::new();
        for (opcode, want) in typed_reads {
            let out = p.on_message(&cmd(opcode, &[0x00]));
            let (got, ret) = parse_complete(&out[0]);
            assert_eq!(got, opcode);
            assert_eq!(
                ret.len(),
                want,
                "opcode {opcode:#06x} returned {} bytes, expected {want}",
                ret.len()
            );
            assert_eq!(ret[0], STATUS_SUCCESS, "opcode {opcode:#06x} was refused");
        }
        assert!(
            p.unmodelled().is_empty(),
            "all typed reads must be modelled: {:?}",
            p.unmodelled()
        );
    }

    /// Regression test for a second real Bluetooth-process abort, found by the fallback's own log line.
    ///
    /// `HCI_Set_Min_Encryption_Key_Size` (0x0c84) fell through to UNKNOWN_COMMAND, and the stack does
    /// not tolerate that: `controller_impl.cc:437 set_min_encryption_key_size_handler:
    /// Status UNKNOWN_HCI_COMMAND(0x01)` aborts the process. Refusal is not a safe default for
    /// commands the stack considers mandatory.
    #[test]
    fn set_min_encryption_key_size_must_succeed_not_be_refused() {
        let mut p = BtPort::new();
        let out = p.on_message(&cmd(op::SET_MIN_ENCRYPTION_KEY_SIZE, &[0x07]));
        let (opcode, ret) = parse_complete(&out[0]);
        assert_eq!(opcode, op::SET_MIN_ENCRYPTION_KEY_SIZE);
        assert_eq!(
            ret,
            vec![STATUS_SUCCESS],
            "a refusal here aborts the guest's Bluetooth process"
        );
        assert!(p.unmodelled().is_empty());
    }

    /// The fallback must record what it refused, so one boot enumerates every remaining gap rather than
    /// leaving us guessing at the HCI surface.
    #[test]
    fn the_fallback_records_unmodelled_opcodes_once_each() {
        let mut p = BtPort::new();
        p.on_message(&cmd(0x0c99, &[]));
        p.on_message(&cmd(0x0c99, &[]));
        p.on_message(&cmd(0x0c98, &[]));
        assert_eq!(p.unmodelled(), &[0x0c99, 0x0c98]);
    }

    /// Refusing a vendor-specific command is correct behaviour, not a gap, so it must not be recorded
    /// as one — otherwise the regression gate flags a controller that is behaving exactly right.
    #[test]
    fn vendor_specific_opcodes_are_refused_without_being_recorded_as_gaps() {
        let mut p = BtPort::new();
        // OGF 0x3f = vendor-specific.
        let out = p.on_message(&cmd(0xfd53, &[]));
        let (opcode, ret) = parse_complete(&out[0]);
        assert_eq!(opcode, 0xfd53);
        assert_eq!(ret, vec![STATUS_UNKNOWN_COMMAND]);
        assert!(
            p.unmodelled().is_empty(),
            "a vendor command is not an unmodelled gap: {:?}",
            p.unmodelled()
        );
    }

    /// Every opcode the live guest was observed issuing must now be handled. These were enumerated by
    /// the unmodelled-opcode log across successive boots rather than guessed at from the spec.
    #[test]
    fn every_opcode_the_live_guest_issued_is_modelled() {
        let observed = [
            0x080fu16, 0x0c18, 0x0c1c, 0x0c1e, 0x0c26, 0x0c43, 0x0c45, 0x0c47, 0x0c52, 0x0c5a,
            0x0c5b, 0x202e,
        ];
        let mut p = BtPort::new();
        for opcode in observed {
            let out = p.on_message(&cmd(opcode, &[0x00, 0x00]));
            let (got, ret) = parse_complete(&out[0]);
            assert_eq!(got, opcode);
            assert_eq!(
                ret.first().copied(),
                Some(STATUS_SUCCESS),
                "opcode {opcode:#06x} was refused"
            );
        }
        assert!(
            p.unmodelled().is_empty(),
            "still unmodelled: {:?}",
            p.unmodelled()
        );
    }

    #[test]
    fn fuzz_lite_never_panics() {
        let mut x: u32 = 0x1357_9bdf;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut p = BtPort::new();
        for _ in 0..200_000 {
            let b = (next() & 0xff) as u8;
            let _ = p.on_message(&[b]);
        }
        // A command claiming 255 parameter bytes but delivering none must stay buffered.
        let mut q = BtPort::new();
        assert!(q.on_message(&[H4_COMMAND, 0x03, 0x0c, 0xff]).is_empty());
        // An ACL packet with an absurd length must resync rather than allocate.
        let mut r = BtPort::new();
        let mut acl = vec![H4_ACL, 0x00, 0x00];
        acl.extend_from_slice(&0xffffu16.to_le_bytes());
        assert!(r.on_message(&acl).is_empty());
        assert!(r.desynced());
    }
}
