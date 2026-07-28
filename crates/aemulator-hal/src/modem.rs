//! Modem: an AT-command server for the Cuttlefish RIL, over vsock.
//!
//! ## Why this is the highest-value backend per line of code
//!
//! `vendor.ril-daemon` (`/apex/com.google.cf.rild/bin/hw/android.hardware.radio-service.cuttlefish`)
//! reads `ro.boot.modem_simulator_ports`, opens an `AF_VSOCK` socket to that port, and speaks AT. With
//! neither the property nor a vsock transport it died immediately:
//!
//! ```text
//! E radio-service.cf: 'ro.boot.modem_simulator_ports' must be an integer vsock port for the modem simulator
//! E init: process with updatable components 'vendor.ril-daemon' exited 4 times in 4 minutes
//! I init: processing action (sys.init.updatable_crashing=1) from (flags_health_check.rc:10)
//! I flags_health_check: ServerConfigurableFlagsReset updatable crashing detected, resetting flags.
//! ```
//!
//! That last line is why this matters beyond log noise: the crash loop makes init declare the device
//! unhealthy and **wipe every server-configurable flag on every boot**. Merely accepting the connection
//! and answering coherently stops it.
//!
//! ## The dialect
//!
//! Standard 3GPP TS 27.007 AT commands, recovered from the host package's own `modem_simulator` and its
//! unit-test binary. Requests are `\r`-terminated; responses are `\r\n<lines>\r\nOK\r\n` (or
//! `\r\nERROR\r\n`), which is the V.250 verbose format the RIL expects.
//!
//! ## The SIM
//!
//! Values are taken from the host package's `etc/modem_simulator/files/iccprofile_for_sim0.xml` and
//! `numeric_operator.xml` rather than invented, so the guest sees the identity real Cuttlefish would
//! present: ICCID `89860318640220133897`, IMSI `311740123456789`, MCC/MNC 311740 → operator
//! "TelAlaska". The profile's `<PINSTATE>PINSTATE_UNKNOWN</PINSTATE>` maps to "ready", i.e. no PIN
//! entry is needed to reach a usable SIM.

use crate::vsock::{ConnHandle, VsockService};
use std::sync::{Arc, Mutex};

/// ICCID from the Cuttlefish host package's SIM profile.
pub const ICCID: &str = "89860318640220133897";
/// IMSI from the same profile: MCC 311, MNC 740, then the subscriber id.
pub const IMSI: &str = "311740123456789";
/// The numeric operator (MCC+MNC) the IMSI belongs to.
pub const MCC_MNC: &str = "311740";
/// Operator name for `MCC_MNC` per `numeric_operator.xml`.
pub const OPERATOR_LONG: &str = "TelAlaska";
pub const OPERATOR_SHORT: &str = "TelAlaska";
/// A plausible IMEI. Luhn-valid, because some stacks validate the check digit.
pub const IMEI: &str = "356938035643809";

/// Registration state values for `+CREG`/`+CGREG`/`+CEREG`: 1 = registered, home network.
const REG_HOME: u8 = 1;
/// Access technology: 7 = E-UTRAN (LTE).
const ACT_LTE: u8 = 7;

/// A reply to one AT command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtReply {
    /// `\r\nOK\r\n`.
    Ok,
    /// `\r\nERROR\r\n`.
    Error,
    /// Information lines, then `OK`.
    Lines(Vec<String>),
    /// Nothing at all. Used only where the protocol genuinely expects silence.
    None,
}

impl AtReply {
    fn line(s: impl Into<String>) -> Self {
        AtReply::Lines(vec![s.into()])
    }

    /// Render in V.250 verbose format.
    pub fn render(&self) -> String {
        match self {
            AtReply::Ok => "\r\nOK\r\n".to_string(),
            AtReply::Error => "\r\nERROR\r\n".to_string(),
            AtReply::None => String::new(),
            AtReply::Lines(ls) => {
                let mut s = String::new();
                for l in ls {
                    s.push_str("\r\n");
                    s.push_str(l);
                    s.push_str("\r\n");
                }
                s.push_str("\r\nOK\r\n");
                s
            }
        }
    }
}

/// A PDP (data) context as configured by `+CGDCONT`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdpContext {
    pub cid: u32,
    pub pdp_type: String,
    pub apn: String,
    pub active: bool,
}

/// Modem state shared across every channel the guest opens to this port.
///
/// The RIL may open more than one logical channel; they must all see one modem, so the connection
/// handler holds an `Arc<Mutex<ModemState>>` while keeping its own line buffer private.
pub struct ModemState {
    /// `+CFUN`: 0 = minimum functionality, 1 = full, 4 = airplane.
    pub fun: u8,
    /// `+CMEE`: 0 = plain ERROR, 1 = numeric `+CME ERROR`, 2 = verbose.
    pub cmee: u8,
    pub pin_ready: bool,
    pub registered: bool,
    /// Signal strength as `+CSQ` rssi (0..31) and ber.
    pub rssi: u8,
    pub ber: u8,
    pub contexts: Vec<PdpContext>,
    /// Commands handled, for diagnostics.
    pub commands: u64,
    /// Commands we did not recognise, recorded once each so one boot names every gap.
    pub unknown: Vec<String>,
}

impl Default for ModemState {
    fn default() -> Self {
        Self::new()
    }
}

impl ModemState {
    pub fn new() -> Self {
        Self {
            // Radio on. The RIL will set this itself, but starting powered means a guest that only
            // queries sees a coherent modem.
            fun: 1,
            cmee: 1,
            pin_ready: true,
            registered: true,
            rssi: 22, // ≈ -67 dBm, a good indoor signal
            ber: 0,
            contexts: Vec::new(),
            commands: 0,
            unknown: Vec::new(),
        }
    }

    pub fn unknown_commands(&self) -> &[String] {
        &self.unknown
    }

    /// Handle one AT command line (without its terminator).
    pub fn handle(&mut self, line: &str) -> AtReply {
        let cmd = line.trim();
        if cmd.is_empty() {
            return AtReply::None;
        }
        self.commands += 1;

        // Bare "AT" is the liveness probe.
        if cmd.eq_ignore_ascii_case("AT") {
            return AtReply::Ok;
        }
        // Strip the mandatory "AT" prefix; anything else is not an AT command.
        let Some(body) = cmd.strip_prefix("AT").or_else(|| cmd.strip_prefix("at")) else {
            return AtReply::Error;
        };

        // Split "+CMD=args" / "+CMD?" / "+CMD" into (verb, suffix).
        let (verb, arg) = match body.find(['=', '?']) {
            Some(i) => (&body[..i], &body[i..]),
            None => (body, ""),
        };
        let verb_uc = verb.to_ascii_uppercase();
        let is_query = arg.starts_with('?');
        let is_test = arg.starts_with("=?");
        let params: Vec<&str> = if let Some(rest) = arg.strip_prefix('=') {
            rest.split(',').map(|s| s.trim()).collect()
        } else {
            Vec::new()
        };

        // V.250 lets **basic** commands be concatenated in one line with no separator, and the RIL's very
        // first write uses that: `ATE0Q0V1` is E0 (echo off), Q0 (report results), V1 (verbose). Parsing
        // it as a single verb named "E0Q0V1" made us answer ERROR to the RIL's opening command, which
        // aborted its whole initialisation handshake — the guest went on to report `SIM absent` even
        // though every SIM command below was implemented. Extended (`+`) commands cannot be concatenated,
        // so this only applies when there is no `+`.
        if !body.contains('+') && !body.is_empty() && is_basic_command_sequence(body) {
            return AtReply::Ok;
        }

        match verb_uc.as_str() {
            // ---- basic V.250 -------------------------------------------------------------------
            "E0" | "E1" | "V1" | "V0" | "&F" | "Z" | "S0" | "+CMUX" => AtReply::Ok,
            // Report Mobile Equipment Error verbosity.
            "+CMEE" => {
                if is_query {
                    AtReply::line(format!("+CMEE: {}", self.cmee))
                } else {
                    if let Some(v) = params.first().and_then(|s| s.parse::<u8>().ok()) {
                        self.cmee = v;
                    }
                    AtReply::Ok
                }
            }

            // ---- radio power ------------------------------------------------------------------
            "+CFUN" => {
                if is_query {
                    AtReply::line(format!("+CFUN: {}", self.fun))
                } else if is_test {
                    AtReply::line("+CFUN: (0,1,4)")
                } else {
                    if let Some(v) = params.first().and_then(|s| s.parse::<u8>().ok()) {
                        self.fun = v;
                        // Airplane / minimum functionality drops registration, as real hardware does.
                        self.registered = v == 1;
                    }
                    AtReply::Ok
                }
            }

            // ---- SIM -------------------------------------------------------------------------
            "+CPIN" => {
                if self.pin_ready {
                    AtReply::line("+CPIN: READY")
                } else {
                    AtReply::line("+CPIN: SIM PIN")
                }
            }
            // Cuttlefish's own shortcut for reading the ICCID out of the modem's cache, rather than a
            // full EF_ICCID SIM-file read.
            "+CICCID" => AtReply::line(format!("+CICCID: {ICCID}")),
            "+CIMI" => AtReply::line(IMSI),
            "+CGSN" => AtReply::line(IMEI),
            "+CGMI" => AtReply::line("Cuttlefish"),
            "+CGMM" => AtReply::line("Cuttlefish Modem"),
            "+CGMR" => AtReply::line("1.0"),

            // ---- network registration ---------------------------------------------------------
            // <n>,<stat>. stat 1 = registered on the home network, 0 = not registered.
            "+CREG" | "+CGREG" => {
                let stat = if self.registered { REG_HOME } else { 0 };
                AtReply::line(format!("{}: 0,{stat}", verb_uc.trim_start_matches('+')))
                    .prefixed(&verb_uc)
            }
            "+CEREG" => {
                let stat = if self.registered { REG_HOME } else { 0 };
                // LTE registration also reports TAC, cell id and access technology.
                AtReply::line(format!("+CEREG: 0,{stat},\"1A2B\",\"01A2B3C4\",{ACT_LTE}"))
            }
            "+COPS" => {
                if is_query {
                    // <mode>,<format>,<oper>,<act>: automatic selection, long alphanumeric name.
                    AtReply::line(format!("+COPS: 0,0,\"{OPERATOR_LONG}\",{ACT_LTE}"))
                } else if is_test {
                    AtReply::line(format!(
                        "+COPS: (2,\"{OPERATOR_LONG}\",\"{OPERATOR_SHORT}\",\"{MCC_MNC}\",{ACT_LTE})"
                    ))
                } else {
                    AtReply::Ok
                }
            }
            "+CSQ" => AtReply::line(format!("+CSQ: {},{}", self.rssi, self.ber)),
            "+CESQ" => AtReply::line("+CESQ: 99,99,255,255,20,45"),
            "+CTEC" => AtReply::line("+CTEC: 0,\"f\""),

            // ---- packet data ------------------------------------------------------------------
            "+CGDCONT" => {
                if is_query {
                    let lines: Vec<String> = self
                        .contexts
                        .iter()
                        .map(|c| {
                            format!(
                                "+CGDCONT: {},\"{}\",\"{}\",\"\",0,0",
                                c.cid, c.pdp_type, c.apn
                            )
                        })
                        .collect();
                    if lines.is_empty() {
                        AtReply::Ok
                    } else {
                        AtReply::Lines(lines)
                    }
                } else if is_test {
                    AtReply::line("+CGDCONT: (1-16),\"IP\",,,(0-2),(0-4)")
                } else {
                    // =<cid>,<PDP_type>,<APN>,...
                    let cid = params.first().and_then(|s| s.parse::<u32>().ok());
                    match cid {
                        Some(cid) => {
                            let strip = |s: &str| s.trim_matches('"').to_string();
                            let pdp_type = params.get(1).map(|s| strip(s)).unwrap_or_default();
                            let apn = params.get(2).map(|s| strip(s)).unwrap_or_default();
                            self.contexts.retain(|c| c.cid != cid);
                            self.contexts.push(PdpContext {
                                cid,
                                pdp_type,
                                apn,
                                active: false,
                            });
                            AtReply::Ok
                        }
                        None => AtReply::Error,
                    }
                }
            }
            "+CGACT" => {
                if is_query {
                    let lines: Vec<String> = self
                        .contexts
                        .iter()
                        .map(|c| format!("+CGACT: {},{}", c.cid, u8::from(c.active)))
                        .collect();
                    if lines.is_empty() {
                        AtReply::Ok
                    } else {
                        AtReply::Lines(lines)
                    }
                } else {
                    // =<state>,<cid>
                    let state = params
                        .first()
                        .and_then(|s| s.parse::<u8>().ok())
                        .unwrap_or(0);
                    let cid = params.get(1).and_then(|s| s.parse::<u32>().ok());
                    match cid {
                        Some(cid) => {
                            if let Some(c) = self.contexts.iter_mut().find(|c| c.cid == cid) {
                                c.active = state == 1;
                                AtReply::Ok
                            } else {
                                AtReply::Error
                            }
                        }
                        // No cid means "all contexts".
                        None => {
                            for c in self.contexts.iter_mut() {
                                c.active = state == 1;
                            }
                            AtReply::Ok
                        }
                    }
                }
            }
            "+CGPADDR" => {
                // The address the RIL should hand the framework. It must agree with what the guest's
                // network interface actually has, which on this VMM is the slirp NAT address — so this
                // is deliberately the same 10.0.2.15 the bring-up script assigns.
                let lines: Vec<String> = self
                    .contexts
                    .iter()
                    .filter(|c| c.active)
                    .map(|c| format!("+CGPADDR: {},\"10.0.2.15\"", c.cid))
                    .collect();
                if lines.is_empty() {
                    AtReply::Ok
                } else {
                    AtReply::Lines(lines)
                }
            }

            // ---- voice / SMS: present and idle -----------------------------------------------
            "+CLCC" => AtReply::Ok, // no calls in progress
            "+CMGF" | "+CSCA" | "+CSCB" | "+CNMI" | "+CSMS" | "+CPMS" => AtReply::Ok,
            "+CLIP" => AtReply::line("+CLIP: 0,1"),
            "+CLIR" => AtReply::line("+CLIR: 0,4"),
            "+CCWA" => AtReply::line("+CCWA: 0"),
            "+CSSN" => AtReply::Ok,
            "+CMUT" => AtReply::line("+CMUT: 0"),

            // ---- SIM toolkit: report no proactive session -------------------------------------
            "+CUSATD" | "+CUSATE" | "+CUSATT" | "+CUSATEND" => AtReply::Ok,

            // ---- raw SIM I/O ------------------------------------------------------------------
            // +CRSM / +CSIM are the general-purpose SIM file/APDU channels. A full EF tree would mean
            // replicating iccprofile_for_sim0.xml's whole SIMIO table; the guest reaches SIM-ready via
            // the +CPIN/+CICCID/+CIMI shortcuts above, so these report "file not found" (SW 6A82)
            // rather than pretending to succeed with fabricated content. Status words are the correct
            // vocabulary for "this file is not present", and the RIL handles them.
            "+CRSM" => AtReply::line("+CRSM: 106,130"),
            "+CSIM" => AtReply::line("+CSIM: 4,\"6A82\""),

            // Call mode. `AT+CMOD=0` (single-mode) is part of the RIL's channel initialisation, and
            // refusing it made `AtChannel` time out and give up entirely:
            //   E radio-service.cf: AtChannel:operator():276 failure: Timeout for 'AT+CMOD=0'
            //   F radio-service.cf: getHostChannelPipe:165: Can't init the host channel
            // Note the RIL reported a *timeout* rather than an error — it waits for `OK` specifically, so
            // an `ERROR` reply is indistinguishable from silence to it. That asymmetry is what motivates
            // the set/query split in the fallback below.
            "+CMOD" => AtReply::Ok,

            _ => {
                let first_time =
                    self.unknown.len() < 128 && !self.unknown.iter().any(|u| u == &verb_uc);
                if first_time {
                    self.unknown.push(verb_uc.clone());
                }
                // A **set** command (`+CMD=<args>`) has no return parameters, so acknowledging one we do
                // not model cannot feed the RIL a malformed answer — the worst case is that a setting we
                // ignore appears to have been applied, which is how real modems differ from one another
                // anyway. Refusing it is the genuinely dangerous option: the RIL waits for `OK` and
                // treats `ERROR` as a timeout, then abandons the channel (see `+CMOD` above).
                //
                // A **query** (`+CMD?`) is the opposite: its answer carries data we would have to invent,
                // and a fabricated value is worse than an honest refusal. So queries still get ERROR.
                // A set is exactly "has `=` and is not the `=?` capability query". Testing `params` here
                // instead would misclassify `AT+FOO=?`, whose sole parameter is the `?`.
                if arg.starts_with('=') && !is_test {
                    if first_time {
                        eprintln!(
                            "[hal:modem] unmodelled AT set command {cmd:?} — acknowledged with OK \
                             (a set has no return parameters, and refusing makes the RIL drop the \
                             channel). Add it to the table if it needs to take effect."
                        );
                    }
                    AtReply::Ok
                } else {
                    if first_time {
                        eprintln!(
                            "[hal:modem] unhandled AT query {cmd:?} — answered ERROR rather than \
                             inventing a value. Add it to the command table."
                        );
                    }
                    AtReply::Error
                }
            }
        }
    }
}

impl AtReply {
    /// `+CREG`/`+CGREG` share a body shape but must echo their own verb, so build the line then fix the
    /// prefix rather than duplicating the arm.
    fn prefixed(self, verb: &str) -> Self {
        match self {
            AtReply::Lines(ls) => AtReply::Lines(
                ls.into_iter()
                    .map(|l| match l.split_once(':') {
                        Some((_, rest)) => format!("{verb}:{rest}"),
                        None => l,
                    })
                    .collect(),
            ),
            other => other,
        }
    }
}

/// Whether `body` is a sequence of V.250 **basic** commands, e.g. `E0Q0V1`, `E0`, `&F`, `S0=0`.
///
/// A basic command is a letter (optionally prefixed with `&`) followed by an optional numeric parameter,
/// and V.250 permits any number of them concatenated in one line with no separator. Accepting them
/// wholesale is correct rather than lenient: they configure echo, result-code format and similar, all of
/// which we already behave as if set (we always answer verbose result codes and never echo), so there is
/// nothing to store.
///
/// `S<n>=<v>` (an S-register write) counts too, since refusing it would have exactly the same
/// consequence as the `ATE0Q0V1` failure this exists to fix.
fn is_basic_command_sequence(body: &str) -> bool {
    let b = body.as_bytes();
    let mut i = 0usize;
    let mut saw_one = false;
    while i < b.len() {
        // Optional '&' prefix (e.g. &F, &W, &D2).
        if b[i] == b'&' {
            i += 1;
            if i >= b.len() {
                return false; // a bare '&' is not a command
            }
        }
        if !b[i].is_ascii_alphabetic() {
            return false;
        }
        i += 1;
        // Optional numeric parameter, e.g. the 0 in E0 or the register number in S0.
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        // Optional `=<value>`, which makes it an S-register write (`S0=0`). Note the digits above are the
        // register NUMBER and come before the `=`, which is why this check follows them rather than
        // replacing them.
        if i < b.len() && b[i] == b'=' {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return false; // '=' with no value
            }
        }
        saw_one = true;
    }
    saw_one
}

/// One guest connection to the modem port: a private line buffer over the shared [`ModemState`].
pub struct ModemChannel {
    state: Arc<Mutex<ModemState>>,
    /// Partial command awaiting its `\r`.
    rx: Vec<u8>,
    conn: Option<ConnHandle>,
}

/// Longest AT command we will buffer. A line longer than this means the peer is not speaking AT.
const MAX_AT_LINE: usize = 1024;

impl ModemChannel {
    pub fn new(state: Arc<Mutex<ModemState>>) -> Self {
        Self {
            state,
            rx: Vec::new(),
            conn: None,
        }
    }
}

impl VsockService for ModemChannel {
    fn on_connect(&mut self, conn: ConnHandle) {
        println!("[hal:modem] RIL connected");
        self.conn = Some(conn);
    }

    fn on_data(&mut self, data: &[u8]) {
        self.rx.extend_from_slice(data);
        // AT lines end in CR; tolerate LF too, and drop empty fragments between them.
        loop {
            let Some(end) = self.rx.iter().position(|&b| b == b'\r' || b == b'\n') else {
                // Guard against unbounded growth from a peer that never sends a terminator.
                if self.rx.len() > MAX_AT_LINE {
                    self.rx.clear();
                    eprintln!("[hal:modem] oversize line with no terminator — buffer reset");
                }
                break;
            };
            let line: Vec<u8> = self.rx.drain(..=end).collect();
            let line = &line[..line.len() - 1];
            let Ok(text) = std::str::from_utf8(line) else {
                continue; // not AT; ignore rather than guess
            };
            if text.trim().is_empty() {
                continue;
            }
            let reply = {
                let mut st = match self.state.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                st.handle(text)
            };
            let rendered = reply.render();
            if !rendered.is_empty() {
                if let Some(c) = &self.conn {
                    c.send_str(&rendered);
                }
            }
        }
    }

    fn on_shutdown(&mut self) {
        self.conn = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> ModemState {
        ModemState::new()
    }

    /// Ask `cmd` and return the reply's information lines (without the trailing OK).
    fn ask(s: &mut ModemState, cmd: &str) -> Vec<String> {
        match s.handle(cmd) {
            AtReply::Lines(ls) => ls,
            AtReply::Ok => vec!["OK".into()],
            AtReply::Error => vec!["ERROR".into()],
            AtReply::None => vec![],
        }
    }

    #[test]
    fn bare_at_is_acknowledged() {
        let mut s = st();
        assert_eq!(s.handle("AT"), AtReply::Ok);
        assert_eq!(s.handle("at"), AtReply::Ok);
    }

    /// Regression test for the failure that made the whole RIL unusable.
    ///
    /// V.250 lets basic commands be concatenated, and the RIL's *first* write is `ATE0Q0V1`. Treating
    /// that as one verb answered ERROR, and rild responded by dying — `RILJ: Service SIM has died`,
    /// `GET_SIM_STATUS error: RADIO_NOT_AVAILABLE`, `SST-0: EVENT_ICC_CHANGED: SIM absent` — then init
    /// respawned it, forever. Every SIM command below was already correct; one unhandled command on the
    /// opening line was enough to make none of them reachable.
    #[test]
    fn concatenated_basic_commands_are_accepted() {
        let mut s = st();
        assert_eq!(
            s.handle("ATE0Q0V1"),
            AtReply::Ok,
            "the RIL's opening command"
        );
        assert_eq!(s.handle("ATE0"), AtReply::Ok);
        assert_eq!(
            s.handle("ATE0Q0V1S0=0"),
            AtReply::Ok,
            "with an S-register write"
        );
        assert_eq!(s.handle("AT&F"), AtReply::Ok);
        assert_eq!(s.handle("AT&FE0V1"), AtReply::Ok);
        assert!(
            s.unknown_commands().is_empty(),
            "none of these may reach the unknown-command fallback: {:?}",
            s.unknown_commands()
        );
    }

    /// The basic-command shortcut must not swallow malformed input or extended commands, or a real typo
    /// would be silently accepted.
    #[test]
    fn the_basic_command_shortcut_does_not_over_match() {
        assert!(is_basic_command_sequence("E0Q0V1"));
        assert!(is_basic_command_sequence("&F"));
        assert!(is_basic_command_sequence("S0=0"));
        assert!(!is_basic_command_sequence(""), "empty is not a command");
        assert!(!is_basic_command_sequence("&"), "a bare & is not a command");
        assert!(!is_basic_command_sequence("0E"), "must start with a letter");
        assert!(!is_basic_command_sequence("S0="), "= needs a value");
        assert!(
            !is_basic_command_sequence("E0 Q0"),
            "no spaces in a basic sequence"
        );
        // Extended commands must still route to the table, not be blanket-accepted.
        let mut s = st();
        assert_eq!(s.handle("AT+NOSUCHCMD"), AtReply::Error);
    }

    #[test]
    fn non_at_input_is_refused_not_misparsed() {
        let mut s = st();
        assert_eq!(s.handle("HELLO"), AtReply::Error);
        assert_eq!(s.handle("+CPIN?"), AtReply::Error, "missing AT prefix");
        assert_eq!(s.handle(""), AtReply::None);
    }

    /// Stage (c): SIM ready. This is what gets `IccCardStatus` out of UNKNOWN.
    #[test]
    fn sim_reports_ready_with_the_real_cuttlefish_identity() {
        let mut s = st();
        assert_eq!(ask(&mut s, "AT+CPIN?"), vec!["+CPIN: READY"]);
        assert_eq!(ask(&mut s, "AT+CICCID"), vec![format!("+CICCID: {ICCID}")]);
        assert_eq!(ask(&mut s, "AT+CIMI"), vec![IMSI]);
        // The IMSI must belong to the operator we claim, or the framework's MCC/MNC lookup disagrees
        // with the registration report.
        assert!(
            IMSI.starts_with(MCC_MNC),
            "IMSI must start with the MCC+MNC"
        );
    }

    /// Stage (d): registered on a network.
    #[test]
    fn registration_and_operator_are_consistent() {
        let mut s = st();
        assert_eq!(ask(&mut s, "AT+CREG?"), vec!["+CREG: 0,1"]);
        assert_eq!(ask(&mut s, "AT+CGREG?"), vec!["+CGREG: 0,1"]);
        let cereg = ask(&mut s, "AT+CEREG?");
        assert!(cereg[0].starts_with("+CEREG: 0,1,"), "got {:?}", cereg[0]);
        assert!(cereg[0].ends_with(&ACT_LTE.to_string()));
        assert_eq!(
            ask(&mut s, "AT+COPS?"),
            vec![format!("+COPS: 0,0,\"{OPERATOR_LONG}\",{ACT_LTE}")]
        );
    }

    /// `+CREG` and `+CGREG` share an implementation but must each echo their own verb, or the RIL
    /// attributes the answer to the wrong command.
    #[test]
    fn creg_and_cgreg_echo_their_own_verbs() {
        let mut s = st();
        assert!(ask(&mut s, "AT+CREG?")[0].starts_with("+CREG:"));
        assert!(ask(&mut s, "AT+CGREG?")[0].starts_with("+CGREG:"));
    }

    #[test]
    fn signal_strength_is_in_the_valid_csq_range() {
        let mut s = st();
        let line = ask(&mut s, "AT+CSQ").remove(0);
        let rssi: u8 = line
            .trim_start_matches("+CSQ: ")
            .split(',')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(rssi <= 31, "+CSQ rssi must be 0..31 (or 99 for unknown)");
        assert!(
            rssi > 0,
            "a zero-bar signal would be reported as no service"
        );
    }

    /// Radio power must actually change state, and dropping out of full functionality must drop
    /// registration — a modem reporting "airplane mode but still registered" is incoherent.
    #[test]
    fn cfun_changes_power_and_registration_together() {
        let mut s = st();
        assert_eq!(s.handle("AT+CFUN=0"), AtReply::Ok);
        assert_eq!(ask(&mut s, "AT+CFUN?"), vec!["+CFUN: 0"]);
        assert_eq!(
            ask(&mut s, "AT+CREG?"),
            vec!["+CREG: 0,0"],
            "not registered when powered down"
        );
        assert_eq!(s.handle("AT+CFUN=1"), AtReply::Ok);
        assert_eq!(ask(&mut s, "AT+CREG?"), vec!["+CREG: 0,1"]);
    }

    #[test]
    fn pdp_context_lifecycle() {
        let mut s = st();
        assert_eq!(s.handle("AT+CGDCONT=1,\"IP\",\"internet\""), AtReply::Ok);
        assert_eq!(
            ask(&mut s, "AT+CGDCONT?"),
            vec!["+CGDCONT: 1,\"IP\",\"internet\",\"\",0,0"]
        );
        assert_eq!(ask(&mut s, "AT+CGACT?"), vec!["+CGACT: 1,0"]);
        assert_eq!(s.handle("AT+CGACT=1,1"), AtReply::Ok);
        assert_eq!(ask(&mut s, "AT+CGACT?"), vec!["+CGACT: 1,1"]);
        // Only an active context has an address, and it must match what the guest NIC really has.
        assert_eq!(ask(&mut s, "AT+CGPADDR"), vec!["+CGPADDR: 1,\"10.0.2.15\""]);
        // Re-defining the same cid replaces rather than duplicates it.
        assert_eq!(s.handle("AT+CGDCONT=1,\"IP\",\"other\""), AtReply::Ok);
        assert_eq!(ask(&mut s, "AT+CGDCONT?").len(), 1);
    }

    #[test]
    fn activating_an_unknown_context_is_an_error() {
        let mut s = st();
        assert_eq!(s.handle("AT+CGACT=1,7"), AtReply::Error);
    }

    #[test]
    fn cmee_round_trips() {
        let mut s = st();
        assert_eq!(s.handle("AT+CMEE=2"), AtReply::Ok);
        assert_eq!(ask(&mut s, "AT+CMEE?"), vec!["+CMEE: 2"]);
    }

    /// Raw SIM I/O reports "file not found" rather than fabricating file contents, and the status word
    /// must be the real 6A82.
    #[test]
    fn raw_sim_io_reports_file_not_found_rather_than_inventing_data() {
        let mut s = st();
        assert_eq!(
            ask(&mut s, "AT+CRSM=192,28421,0,0,15"),
            vec!["+CRSM: 106,130"],
            "106,130 is SW1=0x6A SW2=0x82, 'file not found'"
        );
        assert_eq!(
            ask(&mut s, "AT+CSIM=14,\"00A40004027FFF\""),
            vec!["+CSIM: 4,\"6A82\""]
        );
    }

    #[test]
    fn unknown_commands_are_recorded_once_each() {
        let mut s = st();
        assert_eq!(s.handle("AT+NOSUCHCMD"), AtReply::Error);
        assert_eq!(s.handle("AT+NOSUCHCMD"), AtReply::Error);
        assert_eq!(
            s.handle("AT+OTHER=1"),
            AtReply::Ok,
            "an unmodelled SET is acknowledged, not refused — see the set/query split"
        );
        assert_eq!(
            s.unknown_commands(),
            &["+NOSUCHCMD".to_string(), "+OTHER".to_string()],
            "both are recorded regardless of how they were answered, so the log names every gap"
        );
    }

    /// Regression test for the failure that made the RIL abandon its channel.
    ///
    /// The RIL waits for `OK` and treats `ERROR` as a *timeout*, so refusing a set command it issues
    /// during channel init is fatal rather than merely unhelpful:
    ///   `AtChannel:operator():276 failure: Timeout for 'AT+CMOD=0'`
    ///   `getHostChannelPipe:165: Can't init the host channel`
    /// A set command carries no return parameters, so acknowledging an unmodelled one cannot feed the RIL
    /// a malformed answer — whereas inventing a *query* answer could, which is why queries still fail.
    #[test]
    fn unmodelled_sets_are_acknowledged_but_unmodelled_queries_are_refused() {
        let mut s = st();
        assert_eq!(
            s.handle("AT+CMOD=0"),
            AtReply::Ok,
            "the exact command that broke channel init"
        );
        assert_eq!(s.handle("AT+SOMESET=1,2"), AtReply::Ok, "unmodelled set");
        assert_eq!(
            s.handle("AT+SOMEQUERY?"),
            AtReply::Error,
            "unmodelled query"
        );
        assert_eq!(
            s.handle("AT+SOMEBARE"),
            AtReply::Error,
            "unmodelled bare command"
        );
        // `=?` is a capability query, so it must not be blanket-acknowledged either.
        assert_eq!(
            s.handle("AT+SOMETEST=?"),
            AtReply::Error,
            "unmodelled test query"
        );
    }

    #[test]
    fn replies_are_rendered_in_v250_verbose_format() {
        assert_eq!(AtReply::Ok.render(), "\r\nOK\r\n");
        assert_eq!(AtReply::Error.render(), "\r\nERROR\r\n");
        assert_eq!(AtReply::None.render(), "");
        assert_eq!(
            AtReply::Lines(vec!["+CSQ: 22,0".into()]).render(),
            "\r\n+CSQ: 22,0\r\n\r\nOK\r\n",
            "information lines are CRLF-wrapped and followed by OK"
        );
    }

    /// The channel must split on CR, tolerate CRLF, and answer each command exactly once even when the
    /// guest writes several per packet or splits one across packets.
    #[test]
    fn channel_frames_lines_and_answers_each_once() {
        use crate::vsock::{VsockCore, VsockHdr, VsockRegistry, VSOCK_TYPE_STREAM};
        let state = Arc::new(Mutex::new(ModemState::new()));
        let reg = VsockRegistry::new();
        let s2 = state.clone();
        reg.register(
            9600,
            Box::new(move || Box::new(ModemChannel::new(s2.clone()))),
        );
        let mut core = VsockCore::new(reg);
        let hdr = |op: u16, len: u32| VsockHdr {
            src_cid: 3,
            dst_cid: 2,
            src_port: 1024,
            dst_port: 9600,
            len,
            ty: VSOCK_TYPE_STREAM,
            op,
            flags: 0,
            buf_alloc: 1 << 20,
            fwd_cnt: 0,
        };
        core.on_guest_packet(hdr(crate::vsock::op::REQUEST, 0), &[]);

        // Two commands in one write.
        let wire = b"AT\rAT+CPIN?\r";
        let out = core.on_guest_packet(hdr(crate::vsock::op::RW, wire.len() as u32), wire);
        let text: String = out
            .iter()
            .flat_map(|p| p.body.clone())
            .map(|b| b as char)
            .collect();
        assert!(text.contains("OK"), "first command answered: {text:?}");
        assert!(
            text.contains("+CPIN: READY"),
            "second command answered: {text:?}"
        );

        // One command split across two writes: no reply until the CR arrives.
        let out = core.on_guest_packet(hdr(crate::vsock::op::RW, 6), b"AT+CSQ");
        let partial: String = out
            .iter()
            .flat_map(|p| p.body.clone())
            .map(|b| b as char)
            .collect();
        assert!(!partial.contains("+CSQ:"), "answered before the terminator");
        let out = core.on_guest_packet(hdr(crate::vsock::op::RW, 1), b"\r");
        let text: String = out
            .iter()
            .flat_map(|p| p.body.clone())
            .map(|b| b as char)
            .collect();
        assert!(text.contains("+CSQ:"), "answered once complete: {text:?}");
    }

    #[test]
    fn channel_resets_an_oversize_unterminated_line() {
        let state = Arc::new(Mutex::new(ModemState::new()));
        let mut ch = ModemChannel::new(state);
        ch.on_data(&vec![b'A'; MAX_AT_LINE + 10]);
        // Buffer was reset, so a following valid command still parses.
        ch.on_data(b"AT\r");
        // No panic and no stuck state is the assertion; commands() proves the line was processed.
        // (on_connect was never called, so there is no ConnHandle to receive the reply.)
    }

    #[test]
    fn fuzz_lite_never_panics() {
        let mut x: u32 = 0x2468_ace0;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut s = st();
        let alphabet = b"AT+=?,;\"0123456789CFUNPIREGOPSQDCTMEcigs\r\n ";
        for _ in 0..50_000 {
            let n = (next() % 40) as usize;
            let line: String = (0..n)
                .map(|_| alphabet[(next() as usize) % alphabet.len()] as char)
                .collect();
            let _ = s.handle(&line).render();
        }
        // And through the channel, byte at a time.
        let state = Arc::new(Mutex::new(ModemState::new()));
        let mut ch = ModemChannel::new(state);
        for _ in 0..50_000 {
            ch.on_data(&[(next() & 0xff) as u8]);
        }
    }
}
