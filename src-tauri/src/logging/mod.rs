//! Session_Logger — timestamped recording of session I/O (task 16.1).
//!
//! The Session_Logger records a Session's input and output to a log file, where
//! each entry is prefixed with an ISO-8601 timestamp (design: "Session_Logger &
//! Script_Engine"; Requirement 14). Scope of this task is the logger itself:
//!
//! - [`SessionLogger::start`] validates the log destination and, only if it is
//!   writable, opens/creates the log sink. An invalid or non-writable
//!   destination yields [`LogError::InvalidDestination`] and the Session is
//!   **not** opened with logging enabled (Requirement 14.6, Property 46).
//! - [`SessionLogger::record`] writes a timestamp-prefixed entry for a byte
//!   stream in a given [`Direction`]. Because recording only happens for a
//!   session that has been `start`ed, mid-session enable captures only I/O that
//!   arrives after the enable — never the I/O exchanged before (Requirement
//!   14.2, Property 42). A failing write surfaces [`LogError::WriteFailed`]
//!   while every entry written before the failure is retained (Requirement
//!   14.5, Property 45).
//! - [`SessionLogger::stop`] closes the sink, retaining all entries written up
//!   to that point and recording nothing afterward (Requirement 14.3,
//!   Property 43).
//!
//! Enable/disable mid-session is `start()`/`stop()` in place: the Session_Manager
//! invokes [`SessionLogger::start`] when a profile has logging enabled and the
//! Session reaches the connected state (Requirement 14.1 — that wiring lives in
//! the Session_Manager; this module exposes the entry point it calls), and
//! invokes `start`/`stop` again when the user toggles logging on an active
//! Session.
//!
//! ## Testability seams
//!
//! Two ports keep the logger pure and unit-testable without touching real time
//! or the real filesystem:
//!
//! - [`Clock`] supplies "now" as a [`UnixTimestamp`]. Tests inject a fixed clock
//!   so the ISO-8601 prefix is deterministic (Property 44); production uses
//!   [`SystemClock`].
//! - [`LogSink`] abstracts the write destination. Tests inject an in-memory sink
//!   (and one that fails on demand) to exercise the retain-on-failure behavior
//!   without disk I/O; production uses a file-backed sink opened by
//!   [`FileSinkFactory`].
//!
//! ## Timezone offset decision (no new dependency)
//!
//! Requirement 14.4 / Property 44 require the prefix to include the date, the
//! time to the second, **and a timezone offset**. The Rust standard library can
//! read the wall-clock instant (`SystemTime`) but exposes **no** API for the
//! host's *local* UTC offset, and this crate deliberately avoids adding a time
//! crate (`chrono`/`time`) for a single formatter. We therefore emit timestamps
//! in UTC with the `Z` designator (e.g. `2024-05-02T13:45:09Z`). `Z` is the
//! ISO-8601 / RFC 3339 spelling of the `+00:00` offset, so the prefix satisfies
//! the "timezone offset" requirement while keeping the dependency surface
//! unchanged. The offset is produced by a pure function so a future local-offset
//! source can be slotted in without reshaping the API.

mod format;
mod logger;
mod sink;

pub use format::{format_iso8601_utc, UnixTimestamp};
pub use logger::{Clock, Direction, SessionLogger, SystemClock};
pub use sink::{FileSink, FileSinkFactory, InMemorySink, LogSink, SinkFactory};
