//! Serial Supported_Protocol (Requirement 6).
//!
//! This is the Serial implementation of the Protocol_Engine's uniform
//! interface ([`Protocol`] / [`SessionConn`], task 7.1). It opens a configured
//! serial port, validates its parameters *before* touching any hardware, and
//! enumerates the ports the host currently exposes so the Connection_Manager
//! can offer them when a user configures a Serial Connection_Profile.
//!
//! What this module owns, mapped to Requirement 6:
//!
//! - **Parameter validity (6.1, 6.2).** [`validate_serial_params`] is a pure
//!   function: it accepts a set of serial parameters *if and only if* the port
//!   name is set (non-empty) and the baud/data-bits/parity/stop-bits are each
//!   within their supported sets, and otherwise returns
//!   [`ProtocolError::InvalidParameter`] naming the offending parameter. The
//!   domain enums ([`BaudRate`], [`DataBits`], [`Parity`], [`StopBits`]) already
//!   constrain the latter four to their valid sets by construction, so the only
//!   value this function can reject is an unset (empty) `port_name`; it still
//!   maps every enum to the concrete `serialport` value, which is where an
//!   out-of-set value would surface if the domain ever widened. Validation
//!   happens before [`establish`](Protocol::establish) opens anything, so a
//!   rejected profile is left untouched (6.2).
//! - **Port enumeration (6.3, 6.4).** [`available_port_names`] returns the
//!   names of the serial ports the OS currently reports, or an **empty** list
//!   when there are none. The "no serial ports available" wording is a UI
//!   concern; this layer's contract is simply that the list may be empty, and
//!   the UI renders the indication from that.
//! - **Open / abort (6.1, 6.5).** [`SerialProtocol::establish`] validates, then
//!   opens the port with the mapped settings. If the configured port is not
//!   present, the open is aborted with an error that names the unavailable port
//!   (6.5). On any failure nothing is left half-open: the port handle is only
//!   handed to the returned [`SerialSession`] on the success path (Requirement
//!   8.3).
//!
//! Note on scope: `resize` is a no-op for serial — a serial line has no notion
//! of a terminal character grid to resize — so [`SerialSession::resize`]
//! returns `Ok(())` without doing anything, mirroring how the design treats
//! resolution-driven protocols.

use std::io::{Read, Write};
use std::time::Duration;

use async_trait::async_trait;
use serialport::{self, SerialPort};

use crate::domain::{
    BaudRate, DataBits, Parity, ProtocolError, ProtocolKind, ProtocolParams, SessionState, StopBits,
};

use super::interface::{CredentialHandle, Protocol, ResolvedProfile, SessionConn};

/// The timeout applied to blocking reads/writes on the open port.
///
/// Serial lines are quiet by nature: a `recv` that blocked forever would wedge
/// the session's read loop, so a bounded read timeout lets `recv` return "no
/// bytes this moment" (an empty chunk) and be polled again. The value is a
/// balance between responsiveness and not spinning the CPU.
const SERIAL_IO_TIMEOUT: Duration = Duration::from_millis(50);

/// Concrete, OS-ready serial settings produced by validating a profile's
/// [`ProtocolParams::Serial`].
///
/// Holding the mapped `serialport` values together makes [`establish`] a thin
/// "validate, then open" step and keeps the enum→`serialport` mapping in one
/// pure, testable place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialSettings {
    /// The OS port name/path (e.g. `/dev/ttyUSB0`, `COM3`). Never empty.
    pub port_name: String,
    /// Baud rate in bits per second.
    pub baud: u32,
    /// Data-bit width.
    pub data_bits: serialport::DataBits,
    /// Parity setting.
    pub parity: serialport::Parity,
    /// Stop-bit setting.
    pub stop_bits: serialport::StopBits,
}

/// Map a domain [`BaudRate`] to its bits-per-second value.
fn map_baud(baud: BaudRate) -> u32 {
    match baud {
        BaudRate::B300 => 300,
        BaudRate::B1200 => 1200,
        BaudRate::B2400 => 2400,
        BaudRate::B4800 => 4800,
        BaudRate::B9600 => 9600,
        BaudRate::B19200 => 19200,
        BaudRate::B38400 => 38400,
        BaudRate::B57600 => 57600,
        BaudRate::B115200 => 115200,
    }
}

/// Map domain [`DataBits`] to the `serialport` enum.
fn map_data_bits(data_bits: DataBits) -> serialport::DataBits {
    match data_bits {
        DataBits::Five => serialport::DataBits::Five,
        DataBits::Six => serialport::DataBits::Six,
        DataBits::Seven => serialport::DataBits::Seven,
        DataBits::Eight => serialport::DataBits::Eight,
    }
}

/// Map domain [`Parity`] to the `serialport` enum.
fn map_parity(parity: Parity) -> serialport::Parity {
    match parity {
        Parity::None => serialport::Parity::None,
        Parity::Even => serialport::Parity::Even,
        Parity::Odd => serialport::Parity::Odd,
    }
}

/// Map domain [`StopBits`] to the `serialport` enum.
fn map_stop_bits(stop_bits: StopBits) -> serialport::StopBits {
    match stop_bits {
        StopBits::One => serialport::StopBits::One,
        StopBits::Two => serialport::StopBits::Two,
    }
}

/// Validate a profile's serial parameters, producing OS-ready
/// [`SerialSettings`] (Requirement 6.1, 6.2; Property 17).
///
/// Pure and side-effect free: it opens nothing. It accepts the parameters *if
/// and only if* `port_name` is set (non-empty after trimming) and the
/// baud/data-bits/parity/stop-bits are each within their supported sets — the
/// domain enums guarantee the latter four by construction, so the mapping is
/// total and the only rejectable value here is an unset port. On rejection it
/// returns [`ProtocolError::InvalidParameter`] naming the offending parameter
/// (`port_name`), and the caller performs no state change (6.2).
pub fn validate_serial_params(params: &ProtocolParams) -> Result<SerialSettings, ProtocolError> {
    let ProtocolParams::Serial {
        port_name,
        baud,
        data_bits,
        parity,
        stop_bits,
    } = params
    else {
        return Err(ProtocolError::InvalidParameter {
            parameter: "protocol".to_string(),
            message: "profile is not a serial profile".to_string(),
        });
    };

    // Requirement 6.2: an unset port is rejected, naming the parameter, before
    // anything is opened. A port that is empty or only whitespace is "unset".
    if port_name.trim().is_empty() {
        return Err(ProtocolError::InvalidParameter {
            parameter: "port_name".to_string(),
            message: "serial port must be selected".to_string(),
        });
    }

    // The domain enums already constrain baud/data_bits/parity/stop_bits to
    // their supported sets, so mapping them is total — any future widening of
    // those sets would surface here rather than silently opening a bad port.
    Ok(SerialSettings {
        port_name: port_name.clone(),
        baud: map_baud(*baud),
        data_bits: map_data_bits(*data_bits),
        parity: map_parity(*parity),
        stop_bits: map_stop_bits(*stop_bits),
    })
}

/// The serial ports the host operating system currently reports, by name
/// (Requirement 6.3, 6.4).
///
/// Returns an **empty** vector when no ports are present — the "no serial ports
/// available" indication is left to the UI, whose contract is simply that this
/// list may be empty (see [`no_ports_available`], which names that empty-list
/// state). An OS failure to enumerate is surfaced as
/// [`ProtocolError::Io`] rather than being hidden as an empty list, so the UI
/// can tell "none available" apart from "could not look".
pub fn available_port_names() -> Result<Vec<String>, ProtocolError> {
    match serialport::available_ports() {
        Ok(ports) => Ok(ports.into_iter().map(|p| p.port_name).collect()),
        Err(e) => Err(ProtocolError::Io(format!(
            "failed to enumerate serial ports: {e}"
        ))),
    }
}

/// Whether a list of enumerated port names means "no serial ports available"
/// (Requirement 6.4).
///
/// Pure and total: the "no serial ports available" indication is exactly the
/// empty-list case. Extracting this one-liner gives the UI (and tests) a single
/// named place that defines the contract — an empty [`available_port_names`]
/// result is the no-ports state — without depending on the host's hardware.
///
/// Allowed-dead: consumed by the live serial integration test (spec task
/// 10.4), which runs only in CI against a virtual port, so it has no
/// in-crate caller yet.
#[allow(dead_code)]
pub fn no_ports_available(port_names: &[String]) -> bool {
    port_names.is_empty()
}

/// The Serial [`Protocol`] factory.
///
/// Registering this makes [`ProtocolKind::Serial`] a selectable protocol. It
/// holds no state; each [`establish`](Protocol::establish) opens an independent
/// port.
#[derive(Debug, Default, Clone, Copy)]
pub struct SerialProtocol;

impl SerialProtocol {
    /// Construct the Serial protocol factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Protocol for SerialProtocol {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::Serial
    }

    async fn establish(
        &self,
        cfg: &ResolvedProfile,
        _creds: CredentialHandle,
    ) -> Result<Box<dyn SessionConn>, ProtocolError> {
        // Validate first (6.1, 6.2): nothing is opened unless the parameters
        // are valid, so a rejected profile is left untouched.
        let settings = validate_serial_params(&cfg.profile.protocol_params)?;
        let port = open_port(&settings)?;
        Ok(Box::new(SerialSession {
            port: Some(port),
            port_name: settings.port_name,
            state: SessionState::Connected,
        }))
    }
}

/// Open the serial port described by `settings`, mapping failures to the
/// uniform [`ProtocolError`].
///
/// A missing / unavailable port (6.5) maps to
/// [`ProtocolError::InvalidParameter`] naming the port, so the user sees which
/// port could not be opened; every other open failure is an
/// [`ProtocolError::Io`]. On any error no handle escapes this function, so no
/// half-open port is left behind (Requirement 8.3).
fn open_port(settings: &SerialSettings) -> Result<Box<dyn SerialPort>, ProtocolError> {
    serialport::new(&settings.port_name, settings.baud)
        .data_bits(settings.data_bits)
        .parity(settings.parity)
        .stop_bits(settings.stop_bits)
        .timeout(SERIAL_IO_TIMEOUT)
        .open()
        .map_err(|e| match e.kind() {
            // The configured port is not present on the host (6.5): abort and
            // name the unavailable port.
            serialport::ErrorKind::NoDevice => ProtocolError::InvalidParameter {
                parameter: "port_name".to_string(),
                message: format!("serial port `{}` is unavailable", settings.port_name),
            },
            _ => ProtocolError::Io(format!(
                "failed to open serial port `{}`: {e}",
                settings.port_name
            )),
        })
}

/// A live serial [`SessionConn`] over an open port.
///
/// Owns the OS port handle; dropping or [`close`](SessionConn::close)-ing the
/// session releases it. `recv`/`send` read/write the port; `resize` is a no-op
/// because a serial line has no terminal-size concept.
pub struct SerialSession {
    /// The open port, or `None` once closed. Held in an `Option` so `close` can
    /// drop it (releasing the OS handle) while leaving the session addressable
    /// and `close` idempotent.
    port: Option<Box<dyn SerialPort>>,
    /// The port name, retained for diagnostics in I/O errors.
    port_name: String,
    /// The current session state.
    state: SessionState,
}

#[async_trait]
impl SessionConn for SerialSession {
    async fn send(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        let port = self.port.as_mut().ok_or_else(|| {
            ProtocolError::Io(format!("serial port `{}` is closed", self.port_name))
        })?;
        port.write_all(data)
            .map_err(|e| ProtocolError::Io(format!("serial write failed: {e}")))?;
        port.flush()
            .map_err(|e| ProtocolError::Io(format!("serial flush failed: {e}")))?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        let port = self.port.as_mut().ok_or_else(|| {
            ProtocolError::Io(format!("serial port `{}` is closed", self.port_name))
        })?;
        let mut buf = [0u8; 4096];
        match port.read(&mut buf) {
            Ok(0) => Ok(Some(Vec::new())),
            Ok(n) => Ok(Some(buf[..n].to_vec())),
            // A read timeout is normal on a quiet line: report "no bytes right
            // now" (an empty chunk) rather than EOF, so the read loop polls on.
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(Some(Vec::new())),
            Err(e) => Err(ProtocolError::Io(format!("serial read failed: {e}"))),
        }
    }

    async fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), ProtocolError> {
        // No-op: a serial line has no terminal character grid to resize.
        Ok(())
    }

    async fn close(&mut self) -> Result<(), ProtocolError> {
        // Dropping the handle releases the OS port. Idempotent: closing an
        // already-closed session simply finds `None` and succeeds.
        self.port = None;
        self.state = SessionState::Disconnected;
        Ok(())
    }

    fn state(&self) -> SessionState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ProtocolParams;

    fn serial_params(port_name: &str) -> ProtocolParams {
        ProtocolParams::Serial {
            port_name: port_name.to_string(),
            baud: BaudRate::B115200,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
        }
    }

    #[test]
    fn validate_accepts_a_fully_configured_profile() {
        let settings = validate_serial_params(&serial_params("/dev/ttyUSB0"))
            .expect("a fully set serial profile is valid");
        assert_eq!(settings.port_name, "/dev/ttyUSB0");
        assert_eq!(settings.baud, 115_200);
        assert_eq!(settings.data_bits, serialport::DataBits::Eight);
        assert_eq!(settings.parity, serialport::Parity::None);
        assert_eq!(settings.stop_bits, serialport::StopBits::One);
    }

    #[test]
    fn validate_rejects_an_unset_port_naming_the_parameter() {
        let err =
            validate_serial_params(&serial_params("")).expect_err("an empty port name is rejected");
        match err {
            ProtocolError::InvalidParameter { parameter, .. } => {
                assert_eq!(parameter, "port_name");
            }
            other => panic!("expected InvalidParameter, got {other:?}"),
        }
    }

    #[test]
    fn validate_treats_whitespace_only_port_as_unset() {
        let err = validate_serial_params(&serial_params("   "))
            .expect_err("a whitespace-only port name is rejected");
        assert!(matches!(
            err,
            ProtocolError::InvalidParameter { parameter, .. } if parameter == "port_name"
        ));
    }

    #[test]
    fn validate_rejects_a_non_serial_profile() {
        let err = validate_serial_params(&ProtocolParams::Telnet {})
            .expect_err("a non-serial profile has no serial settings");
        assert!(matches!(
            err,
            ProtocolError::InvalidParameter { parameter, .. } if parameter == "protocol"
        ));
    }

    #[test]
    fn enum_mapping_covers_every_supported_value() {
        // Baud: each supported rate maps to its bps value.
        assert_eq!(map_baud(BaudRate::B300), 300);
        assert_eq!(map_baud(BaudRate::B1200), 1200);
        assert_eq!(map_baud(BaudRate::B2400), 2400);
        assert_eq!(map_baud(BaudRate::B4800), 4800);
        assert_eq!(map_baud(BaudRate::B9600), 9600);
        assert_eq!(map_baud(BaudRate::B19200), 19200);
        assert_eq!(map_baud(BaudRate::B38400), 38400);
        assert_eq!(map_baud(BaudRate::B57600), 57600);
        assert_eq!(map_baud(BaudRate::B115200), 115_200);

        // Data bits.
        assert_eq!(map_data_bits(DataBits::Five), serialport::DataBits::Five);
        assert_eq!(map_data_bits(DataBits::Six), serialport::DataBits::Six);
        assert_eq!(map_data_bits(DataBits::Seven), serialport::DataBits::Seven);
        assert_eq!(map_data_bits(DataBits::Eight), serialport::DataBits::Eight);

        // Parity.
        assert_eq!(map_parity(Parity::None), serialport::Parity::None);
        assert_eq!(map_parity(Parity::Even), serialport::Parity::Even);
        assert_eq!(map_parity(Parity::Odd), serialport::Parity::Odd);

        // Stop bits.
        assert_eq!(map_stop_bits(StopBits::One), serialport::StopBits::One);
        assert_eq!(map_stop_bits(StopBits::Two), serialport::StopBits::Two);
    }

    #[test]
    fn serial_protocol_reports_its_kind() {
        assert_eq!(SerialProtocol::new().kind(), ProtocolKind::Serial);
    }

    #[test]
    fn available_port_names_returns_a_list_or_io_error() {
        // Smoke test: enumeration must not panic and either yields a (possibly
        // empty) list or a structured I/O error. We do not assert on the
        // contents because the host's port set is environment-dependent, and we
        // open nothing.
        match available_port_names() {
            Ok(_names) => {}
            Err(ProtocolError::Io(_)) => {}
            Err(other) => panic!("unexpected error kind from enumeration: {other:?}"),
        }
    }

    // --- Task 10.3: no-ports-available state (Requirement 6.4) ------------

    #[test]
    fn empty_port_list_is_the_no_ports_available_state() {
        // Requirement 6.4: when the host reports no serial ports, the picker
        // shows an empty list *and* a "no serial ports available" indication.
        // This layer's contract for that indication is exactly "the enumerated
        // list is empty", captured by the pure `no_ports_available` helper. We
        // test it deterministically against a constructed empty list so the
        // result never depends on the host's actual hardware.
        let empty: Vec<String> = Vec::new();
        assert!(
            no_ports_available(&empty),
            "an empty port list must represent the no-ports-available state"
        );

        // Conversely, any non-empty list is *not* the no-ports state — one or
        // more ports are present, so the picker renders them rather than the
        // indication.
        let one = vec!["/dev/ttyUSB0".to_string()];
        assert!(
            !no_ports_available(&one),
            "a non-empty port list must not be the no-ports-available state"
        );
        let many = vec!["COM1".to_string(), "COM2".to_string(), "COM3".to_string()];
        assert!(!no_ports_available(&many));
    }

    #[test]
    fn enumeration_feeds_the_no_ports_indication_without_panicking() {
        // The real enumeration is host-dependent, so we assert only the
        // deterministic contract: on success its result flows into
        // `no_ports_available`, which classifies it without panicking. We make
        // no claim about whether this particular host has ports.
        if let Ok(names) = available_port_names() {
            let _is_empty_state = no_ports_available(&names);
        }
    }

    // --- Task 10.2 / Property 17: Serial parameter validity ---------------

    mod properties {
        use super::*;
        use proptest::prelude::*;

        /// Every supported baud rate — the domain enum's full set.
        fn any_baud() -> impl Strategy<Value = BaudRate> {
            prop_oneof![
                Just(BaudRate::B300),
                Just(BaudRate::B1200),
                Just(BaudRate::B2400),
                Just(BaudRate::B4800),
                Just(BaudRate::B9600),
                Just(BaudRate::B19200),
                Just(BaudRate::B38400),
                Just(BaudRate::B57600),
                Just(BaudRate::B115200),
            ]
        }

        /// Every supported data-bit width.
        fn any_data_bits() -> impl Strategy<Value = DataBits> {
            prop_oneof![
                Just(DataBits::Five),
                Just(DataBits::Six),
                Just(DataBits::Seven),
                Just(DataBits::Eight),
            ]
        }

        /// Every supported parity setting.
        fn any_parity() -> impl Strategy<Value = Parity> {
            prop_oneof![Just(Parity::None), Just(Parity::Even), Just(Parity::Odd)]
        }

        /// Every supported stop-bit setting.
        fn any_stop_bits() -> impl Strategy<Value = StopBits> {
            prop_oneof![Just(StopBits::One), Just(StopBits::Two)]
        }

        /// Port names spanning the whole input space, over-weighting the
        /// boundary cases the rule turns on: empty, whitespace-only (which
        /// trims to empty and so counts as unset), and genuinely set names
        /// (including ones padded with surrounding whitespace, which stay set).
        fn any_port_name() -> impl Strategy<Value = String> {
            prop_oneof![
                Just(String::new()),                         // unset
                "[ \t\r\n]{1,8}".prop_map(String::from),     // whitespace-only -> unset
                "[A-Za-z0-9/._-]{1,24}",                     // a plain, set name
                "[ \t]{0,3}[A-Za-z0-9/._-]{1,16}[ \t]{0,3}", // set name w/ padding
                any::<String>(),                             // the full domain
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            // Feature: remote-connection-manager, Property 17: For any serial profile, opening is accepted if and only if the port is set and the baud rate, data bits, parity, and stop bits are each within their supported sets; otherwise the open is rejected identifying the offending parameter and the profile is retained unchanged.
            #[test]
            fn prop17_serial_parameter_validity(
                port_name in any_port_name(),
                baud in any_baud(),
                data_bits in any_data_bits(),
                parity in any_parity(),
                stop_bits in any_stop_bits(),
            ) {
                // The domain enums guarantee baud/data_bits/parity/stop_bits are
                // always within their supported sets by construction, so the
                // only free variable in the bijection is the port name.
                let params = ProtocolParams::Serial {
                    port_name: port_name.clone(),
                    baud,
                    data_bits,
                    parity,
                    stop_bits,
                };
                let result = validate_serial_params(&params);
                let port_is_set = !port_name.trim().is_empty();

                // The biconditional: accepted iff the port is set.
                prop_assert_eq!(result.is_ok(), port_is_set);

                match result {
                    Ok(settings) => {
                        // Accepted: the mapped settings faithfully carry the
                        // configured values and nothing was opened to produce
                        // them (the function is pure).
                        prop_assert_eq!(&settings.port_name, &port_name);
                        prop_assert_eq!(settings.baud, map_baud(baud));
                        prop_assert_eq!(settings.data_bits, map_data_bits(data_bits));
                        prop_assert_eq!(settings.parity, map_parity(parity));
                        prop_assert_eq!(settings.stop_bits, map_stop_bits(stop_bits));
                    }
                    Err(err) => {
                        // Rejected: the error names the offending parameter
                        // (`port_name`) and the profile is left untouched — the
                        // input `params` we still hold is byte-for-byte the one
                        // we passed in, so nothing was mutated or opened.
                        match err {
                            ProtocolError::InvalidParameter { parameter, .. } => {
                                prop_assert_eq!(parameter, "port_name");
                            }
                            other => {
                                prop_assert!(
                                    false,
                                    "expected InvalidParameter(port_name), got {:?}",
                                    other
                                );
                            }
                        }
                        prop_assert_eq!(
                            &params,
                            &ProtocolParams::Serial {
                                port_name,
                                baud,
                                data_bits,
                                parity,
                                stop_bits,
                            }
                        );
                    }
                }
            }

            // Feature: remote-connection-manager, Property 17: For any serial profile, opening is accepted if and only if the port is set and the baud rate, data bits, parity, and stop bits are each within their supported sets; otherwise the open is rejected identifying the offending parameter and the profile is retained unchanged.
            #[test]
            fn prop17_non_serial_profile_is_rejected_as_protocol(
                which in 0u8..3,
                resolution in prop::option::of((640u32..=7680, 480u32..=4320)),
            ) {
                // A profile that is not Serial has no serial settings at all, so
                // validation rejects it naming the `protocol` parameter rather
                // than any serial field — and opens nothing.
                let params = match which {
                    0 => ProtocolParams::Telnet {},
                    1 => ProtocolParams::Ssh {
                        known_host_policy: crate::domain::HostKeyPolicy::Strict,
                    },
                    _ => ProtocolParams::Rdp {
                        resolution: resolution
                            .map(|(width, height)| crate::domain::Resolution { width, height }),
                    },
                };

                let err = validate_serial_params(&params)
                    .expect_err("a non-serial profile has no serial settings");
                match err {
                    ProtocolError::InvalidParameter { parameter, .. } => {
                        prop_assert_eq!(parameter, "protocol");
                    }
                    other => prop_assert!(
                        false,
                        "expected InvalidParameter(protocol), got {:?}",
                        other
                    ),
                }
            }
        }
    }
}
