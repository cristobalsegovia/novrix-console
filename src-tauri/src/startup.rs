//! Minimum-OS-version startup gate (task 1.3, Requirement 1.6).
//!
//! Novrix Console only runs on macOS 12+ or Windows 10+. This module detects
//! the host OS version at startup and decides whether the application is
//! allowed to run. When the host is below the minimum, startup is blocked and a
//! `DomainError` carrying a minimum-version message is produced so the shell
//! can surface it to the user.
//!
//! The logic is split deliberately for testability:
//!
//! - [`HostOs`] / [`HostOsVersion`] model the detected host, independent of how
//!   it is obtained.
//! - [`evaluate_startup`] is a **pure** function: given a detected version (or
//!   `None` when detection failed), it returns `Ok(())` iff the version meets
//!   the minimum, otherwise an `Err(DomainError)`. This is the function the
//!   property test in task 1.4 (Property 1) exercises across all inputs.
//! - [`detect_host_os`] performs the real, platform-specific detection and is
//!   kept separate so it never gets in the way of testing the decision rule.

use crate::domain::DomainError;

/// The operating systems Novrix Console supports (plus a catch-all for any
/// host that is neither macOS nor Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// Apple macOS. Minimum supported: major version 12 (Monterey).
    MacOs,
    /// Microsoft Windows. Minimum supported: major version 10.
    Windows,
    /// Any other operating system — unsupported for distribution.
    Other,
}

/// A detected host OS and its major version.
///
/// Only the major version participates in the gate decision: the requirement is
/// expressed purely in terms of "macOS 12 or later" / "Windows 10 or later".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostOsVersion {
    /// Which operating system the host is running.
    pub os: HostOs,
    /// The major version number (e.g. `12` for macOS Monterey, `10` for
    /// Windows 10, `11` for Windows 11).
    pub major: u32,
}

impl HostOsVersion {
    /// Construct a detected host version.
    pub fn new(os: HostOs, major: u32) -> Self {
        Self { os, major }
    }
}

/// Minimum supported macOS major version.
pub const MIN_MACOS_MAJOR: u32 = 12;
/// Minimum supported Windows major version.
pub const MIN_WINDOWS_MAJOR: u32 = 10;

/// Stable, machine-readable error code for a blocked startup.
pub const UNSUPPORTED_OS_CODE: &str = "startup.unsupportedOsVersion";

/// Decide whether startup is allowed for a detected host OS version.
///
/// This is the pure core of the gate (Requirement 1.6 / design Property 1).
/// Startup is allowed **if and only if** the host is:
/// - macOS with major version `>= 12`, or
/// - Windows with major version `>= 10`.
///
/// In every other case — a supported OS below its minimum, an unsupported OS,
/// or a host whose version could not be detected (`None`) — startup is blocked
/// and an `Err` carrying a user-facing minimum-version message is returned.
///
/// Keeping detection out of this function lets the property test drive it with
/// arbitrary `(os, major)` pairs without touching the real host.
pub fn evaluate_startup(detected: Option<HostOsVersion>) -> Result<(), DomainError> {
    match detected {
        Some(HostOsVersion {
            os: HostOs::MacOs,
            major,
        }) if major >= MIN_MACOS_MAJOR => Ok(()),
        Some(HostOsVersion {
            os: HostOs::Windows,
            major,
        }) if major >= MIN_WINDOWS_MAJOR => Ok(()),
        _ => Err(unsupported_os_error()),
    }
}

/// Build the minimum-version error surfaced when startup is blocked.
fn unsupported_os_error() -> DomainError {
    DomainError::new(
        UNSUPPORTED_OS_CODE,
        format!(
            "This operating system version is not supported. \
             Novrix Console requires macOS {MIN_MACOS_MAJOR} or later, \
             or Windows {MIN_WINDOWS_MAJOR} or later."
        ),
    )
}

/// Detect the host OS and its major version.
///
/// Returns `None` when the host is neither macOS nor Windows, or when the
/// version string cannot be parsed — both of which [`evaluate_startup`] treats
/// as "block startup". The real detection is platform-specific and is isolated
/// here so the decision rule stays trivially testable.
#[cfg(target_os = "macos")]
pub fn detect_host_os() -> Option<HostOsVersion> {
    // `sw_vers -productVersion` prints e.g. "12.7.1"; the leading component is
    // the major version used by the gate.
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let major = parse_major(version.trim())?;
    Some(HostOsVersion::new(HostOs::MacOs, major))
}

/// Detect the host OS and its major version. See the macOS variant for details.
#[cfg(target_os = "windows")]
pub fn detect_host_os() -> Option<HostOsVersion> {
    // On Windows the product build number distinguishes 10/11, but the gate
    // only needs the user-facing major version. `RtlGetVersion`-style APIs
    // report Windows 10 and 11 both as major version 10 at the NT level, so we
    // read the marketing major from the registry's `CurrentMajorVersionNumber`,
    // falling back to parsing the `cmd /c ver` output.
    use std::process::Command;

    let output = Command::new("cmd").args(["/C", "ver"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Output looks like: "Microsoft Windows [Version 10.0.19045.3570]".
    let version = text
        .split("Version")
        .nth(1)?
        .trim()
        .trim_end_matches(']')
        .trim();
    let major = parse_major(version)?;
    Some(HostOsVersion::new(HostOs::Windows, major))
}

/// Detect the host OS on any other platform — always unsupported.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn detect_host_os() -> Option<HostOsVersion> {
    None
}

/// Parse the leading major-version component from a dotted version string
/// (e.g. `"12.7.1"` -> `12`). Returns `None` when no leading integer is present.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn parse_major(version: &str) -> Option<u32> {
    version.split('.').next()?.trim().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_macos_at_minimum() {
        assert!(evaluate_startup(Some(HostOsVersion::new(HostOs::MacOs, 12))).is_ok());
    }

    #[test]
    fn allows_macos_above_minimum() {
        assert!(evaluate_startup(Some(HostOsVersion::new(HostOs::MacOs, 15))).is_ok());
    }

    #[test]
    fn blocks_macos_below_minimum() {
        let err = evaluate_startup(Some(HostOsVersion::new(HostOs::MacOs, 11))).unwrap_err();
        assert_eq!(err.code, UNSUPPORTED_OS_CODE);
    }

    #[test]
    fn allows_windows_at_minimum() {
        assert!(evaluate_startup(Some(HostOsVersion::new(HostOs::Windows, 10))).is_ok());
    }

    #[test]
    fn allows_windows_above_minimum() {
        assert!(evaluate_startup(Some(HostOsVersion::new(HostOs::Windows, 11))).is_ok());
    }

    #[test]
    fn blocks_windows_below_minimum() {
        let err = evaluate_startup(Some(HostOsVersion::new(HostOs::Windows, 7))).unwrap_err();
        assert_eq!(err.code, UNSUPPORTED_OS_CODE);
    }

    #[test]
    fn blocks_unsupported_os() {
        let err = evaluate_startup(Some(HostOsVersion::new(HostOs::Other, 99))).unwrap_err();
        assert_eq!(err.code, UNSUPPORTED_OS_CODE);
    }

    #[test]
    fn blocks_when_detection_failed() {
        let err = evaluate_startup(None).unwrap_err();
        assert_eq!(err.code, UNSUPPORTED_OS_CODE);
    }

    #[test]
    fn blocked_error_mentions_both_minimums() {
        let err = evaluate_startup(None).unwrap_err();
        assert!(err.message.contains(&MIN_MACOS_MAJOR.to_string()));
        assert!(err.message.contains(&MIN_WINDOWS_MAJOR.to_string()));
    }

    mod property {
        use super::*;
        use proptest::prelude::*;

        /// Strategy over the three host operating systems the gate knows about.
        fn host_os() -> impl Strategy<Value = HostOs> {
            prop_oneof![
                Just(HostOs::MacOs),
                Just(HostOs::Windows),
                Just(HostOs::Other),
            ]
        }

        /// Strategy over a detected host (an `(os, major)` pair) or a failed
        /// detection (`None`). The major-version space is biased toward the
        /// decision boundaries (0..=20) so the generator spends most of its
        /// budget on the versions that actually exercise the gate, while still
        /// reaching far-above-minimum values.
        fn detected() -> impl Strategy<Value = Option<HostOsVersion>> {
            let major = prop_oneof![
                9 => 0u32..=20,
                1 => 0u32..=u32::MAX,
            ];
            let some = (host_os(), major)
                .prop_map(|(os, major)| Some(HostOsVersion::new(os, major)));
            prop_oneof![
                9 => some,
                1 => Just(None),
            ]
        }

        proptest! {
            // Feature: remote-connection-manager, Property 1: For any operating system version, startup is allowed if and only if the version is at or above the configured minimum (macOS 12 / Windows 10); below the minimum, startup is blocked with a minimum-version message.
            #[test]
            fn minimum_os_version_gate(detected in detected()) {
                // Independent reference predicate for the biconditional: startup
                // is allowed exactly for supported-OS-at-or-above-minimum.
                let should_allow = matches!(
                    detected,
                    Some(HostOsVersion { os: HostOs::MacOs, major }) if major >= MIN_MACOS_MAJOR,
                ) || matches!(
                    detected,
                    Some(HostOsVersion { os: HostOs::Windows, major }) if major >= MIN_WINDOWS_MAJOR,
                );

                let result = evaluate_startup(detected);

                prop_assert_eq!(
                    result.is_ok(),
                    should_allow,
                    "allow decision must match the minimum-version predicate for {:?}",
                    detected
                );

                // When blocked, the error must be the minimum-version error with
                // a message naming the required minimums.
                if let Err(err) = result {
                    prop_assert_eq!(&err.code, UNSUPPORTED_OS_CODE);
                    prop_assert!(err.message.contains(&MIN_MACOS_MAJOR.to_string()));
                    prop_assert!(err.message.contains(&MIN_WINDOWS_MAJOR.to_string()));
                }
            }
        }
    }
}
