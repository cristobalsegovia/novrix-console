//! The [`SessionLogger`] itself — start/record/stop over an injectable clock
//! and sink factory.
//!
//! The logger holds one open [`LogSink`] per active session. `start` validates
//! and opens the sink (refusing to enable logging on an invalid destination),
//! `record` writes a timestamp-prefixed entry, and `stop` closes the sink. All
//! three are keyed by [`SessionId`], so enabling logging on session A never
//! touches session B.
//!
//! Both time and the write destination are injected:
//! - [`Clock`] provides "now"; [`SystemClock`] reads the wall clock in
//!   production, and tests inject a fixed clock for a deterministic ISO-8601
//!   prefix (Property 44).
//! - [`SinkFactory`](super::SinkFactory) opens the destination; tests inject an
//!   in-memory factory to avoid disk I/O.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::{LogError, SessionId};

use super::format::{format_iso8601_utc, UnixTimestamp};
use super::sink::{LogSink, SinkFactory};

/// The direction of a recorded byte stream: data the user sent to the Session
/// (`Input`) or data the Session produced (`Output`). Requirement 14 logs both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Bytes sent from the user to the remote Session.
    Input,
    /// Bytes received from the remote Session.
    Output,
}

impl Direction {
    /// Short tag used in the log line, between the timestamp and the payload.
    fn tag(self) -> &'static str {
        match self {
            Direction::Input => "IN",
            Direction::Output => "OUT",
        }
    }
}

/// A source of the current time, as seconds since the Unix epoch.
///
/// Injected into the logger so the ISO-8601 prefix is deterministic under test
/// (Property 44) and so the logger has no hidden dependency on the wall clock.
pub trait Clock: Send + Sync {
    /// The current instant as a [`UnixTimestamp`].
    fn now(&self) -> UnixTimestamp;
}

/// Production [`Clock`] reading the host wall clock via `SystemTime`.
///
/// This is the one place the module touches real time; the formatting and the
/// logger logic stay pure and testable behind it. A clock set before the Unix
/// epoch floors to `0` rather than panicking.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixTimestamp {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        UnixTimestamp(secs)
    }
}

/// Records Session I/O to per-session log sinks with ISO-8601 prefixes.
///
/// Generic over the [`Clock`] and the [`SinkFactory`] so production wires a
/// `SystemClock` + file factory while tests wire a fixed clock + in-memory
/// factory. Active sinks live behind a `Mutex` because `record` takes `&self`
/// (matching the design's `fn record(&self, ...)`), yet needs to mutate the
/// open file.
pub struct SessionLogger<C, F>
where
    C: Clock,
    F: SinkFactory,
{
    clock: C,
    factory: F,
    sinks: Mutex<HashMap<SessionId, F::Sink>>,
}

impl<C, F> SessionLogger<C, F>
where
    C: Clock,
    F: SinkFactory,
{
    /// Build a logger from a clock and a sink factory.
    pub fn new(clock: C, factory: F) -> Self {
        Self {
            clock,
            factory,
            sinks: Mutex::new(HashMap::new()),
        }
    }

    /// Begin logging `session` to `dest`.
    ///
    /// The Session_Manager calls this when a profile has logging enabled and the
    /// Session reaches the connected state (Requirement 14.1), and again when a
    /// user enables logging on an active Session (Requirement 14.2). The
    /// destination is validated first: if it does not exist or is not writable,
    /// this returns [`LogError::InvalidDestination`] and **no** sink is opened,
    /// so the Session is not logging-enabled (Requirement 14.6, Property 46).
    ///
    /// Because recording only starts once a sink exists, any I/O exchanged
    /// before this call is never captured (Property 42). Starting again for a
    /// session that is already logging replaces the sink, closing the old one.
    pub fn start(&self, session: SessionId, dest: &Path) -> Result<(), LogError> {
        // Validate + open before touching the active-sink map, so a rejected
        // destination leaves no partial state.
        let sink = self.factory.open(dest)?;

        let mut sinks = self.sinks.lock().unwrap();
        if let Some(mut previous) = sinks.insert(session, sink) {
            // A prior sink for this session is closed, retaining its entries.
            let _ = previous.close();
        }
        Ok(())
    }

    /// Record `bytes` for `session` in the given [`Direction`], prefixed with an
    /// ISO-8601 timestamp (Requirement 14.4, Property 44).
    ///
    /// If `session` is not currently logging (never started, or already
    /// stopped), the call is a no-op — this is how post-disable I/O is dropped
    /// (Property 43) and pre-enable I/O is never seen (Property 42). On a write
    /// failure the entries written before it are retained and
    /// [`LogError::WriteFailed`] is returned (Requirement 14.5, Property 45).
    pub fn record(
        &self,
        session: SessionId,
        direction: Direction,
        bytes: &[u8],
    ) -> Result<(), LogError> {
        let mut sinks = self.sinks.lock().unwrap();
        let Some(sink) = sinks.get_mut(&session) else {
            // Not logging for this session: silently drop (pre-enable /
            // post-disable I/O is never recorded).
            return Ok(());
        };

        let entry = format_entry(self.clock.now(), direction, bytes);
        sink.write_entry(&entry)
    }

    /// Stop logging `session`, closing its sink and retaining every entry
    /// written so far (Requirement 14.3, Property 43). Nothing is recorded for
    /// the session afterward until `start` is called again. Stopping a session
    /// that is not logging is a no-op.
    pub fn stop(&self, session: SessionId) -> Result<(), LogError> {
        let mut sinks = self.sinks.lock().unwrap();
        match sinks.remove(&session) {
            Some(mut sink) => sink.close(),
            None => Ok(()),
        }
    }

    /// Whether `session` currently has logging enabled.
    pub fn is_logging(&self, session: SessionId) -> bool {
        self.sinks.lock().unwrap().contains_key(&session)
    }
}

/// Build one log line: `<ISO-8601> <DIR> <payload>\n`.
///
/// Kept as a free function so the exact framing is unit-testable independent of
/// any sink or clock. The payload is appended verbatim (bytes are not escaped;
/// callers log already-decoded session text).
fn format_entry(now: UnixTimestamp, direction: Direction, bytes: &[u8]) -> Vec<u8> {
    let prefix = format!("{} {} ", format_iso8601_utc(now), direction.tag());
    let mut entry = Vec::with_capacity(prefix.len() + bytes.len() + 1);
    entry.extend_from_slice(prefix.as_bytes());
    entry.extend_from_slice(bytes);
    entry.push(b'\n');
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::sink::InMemorySink;

    /// A clock pinned to a fixed instant for deterministic prefixes.
    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now(&self) -> UnixTimestamp {
            UnixTimestamp(self.0)
        }
    }

    /// A factory handing out a shared [`InMemorySink`] so tests can inspect it
    /// after it is moved into the logger.
    struct InMemoryFactory {
        sink: InMemorySink,
    }
    impl SinkFactory for InMemoryFactory {
        type Sink = InMemorySink;
        fn open(&self, _dest: &Path) -> Result<InMemorySink, LogError> {
            Ok(self.sink.clone())
        }
    }

    /// A factory that always rejects the destination (Property 46).
    struct RejectingFactory;
    impl SinkFactory for RejectingFactory {
        type Sink = InMemorySink;
        fn open(&self, _dest: &Path) -> Result<InMemorySink, LogError> {
            Err(LogError::InvalidDestination("nope".to_string()))
        }
    }

    fn logger_with(
        ts: i64,
        sink: InMemorySink,
    ) -> SessionLogger<FixedClock, InMemoryFactory> {
        SessionLogger::new(FixedClock(ts), InMemoryFactory { sink })
    }

    #[test]
    fn format_entry_carries_iso8601_prefix_and_direction() {
        // 2024-05-02T13:45:09Z.
        let entry = format_entry(UnixTimestamp(1_714_657_509), Direction::Output, b"hello");
        let line = String::from_utf8(entry).unwrap();
        assert_eq!(line, "2024-05-02T13:45:09Z OUT hello\n");
    }

    #[test]
    fn records_only_after_start_and_stops_after_disable() {
        // Property 42 / 43 smoke test: capture only between start and stop.
        let shared = InMemorySink::new();
        let logger = logger_with(0, shared.clone());
        let session = SessionId::new();

        // Pre-enable I/O is dropped (no sink yet).
        logger.record(session, Direction::Input, b"before").unwrap();
        assert!(shared.entries().is_empty());

        logger.start(session, Path::new("ignored.log")).unwrap();
        assert!(logger.is_logging(session));
        logger.record(session, Direction::Input, b"after").unwrap();

        logger.stop(session).unwrap();
        assert!(!logger.is_logging(session));
        assert!(shared.is_closed());

        // Post-disable I/O is dropped.
        logger.record(session, Direction::Output, b"afterstop").unwrap();

        let lines: Vec<String> = shared
            .entries()
            .into_iter()
            .map(|e| String::from_utf8(e).unwrap())
            .collect();
        assert_eq!(lines, vec!["1970-01-01T00:00:00Z IN after\n"]);
    }

    #[test]
    fn invalid_destination_prevents_logging_enabled_start() {
        // Property 46: a rejected destination means the session is not logging.
        let logger = SessionLogger::new(FixedClock(0), RejectingFactory);
        let session = SessionId::new();
        let err = logger.start(session, Path::new("bad.log")).unwrap_err();
        assert!(matches!(err, LogError::InvalidDestination(_)));
        assert!(!logger.is_logging(session));
    }

    #[test]
    fn write_failure_retains_prior_entries() {
        // Property 45: a failing write keeps earlier entries and reports error.
        let shared = InMemorySink::failing_after(1);
        let logger = logger_with(0, shared.clone());
        let session = SessionId::new();
        logger.start(session, Path::new("ignored.log")).unwrap();

        logger.record(session, Direction::Input, b"ok").unwrap();
        let err = logger
            .record(session, Direction::Output, b"boom")
            .unwrap_err();
        assert!(matches!(err, LogError::WriteFailed(_)));

        let lines: Vec<String> = shared
            .entries()
            .into_iter()
            .map(|e| String::from_utf8(e).unwrap())
            .collect();
        assert_eq!(lines, vec!["1970-01-01T00:00:00Z IN ok\n"]);
    }

    mod properties {
        use super::*;
        use proptest::collection::vec;
        use proptest::prelude::*;

        /// One recorded I/O event: a direction plus a short, non-empty,
        /// newline-free payload (payloads carry no `\n` so the one-line-per-entry
        /// framing stays unambiguous when a property reconstructs expected
        /// lines).
        #[derive(Debug, Clone)]
        struct Event {
            direction: Direction,
            payload: Vec<u8>,
        }

        /// Strategy over a single I/O event.
        fn event() -> impl Strategy<Value = Event> {
            let direction = prop_oneof![Just(Direction::Input), Just(Direction::Output)];
            // Printable, newline-free bytes keep the reconstructed expected line
            // comparable to the sink's framing (`<ts> <DIR> <payload>\n`).
            let payload = vec(0x20u8..=0x7eu8, 1..16);
            (direction, payload).prop_map(|(direction, payload)| Event { direction, payload })
        }

        /// The exact line the logger frames for `event` at timestamp `ts`.
        fn expected_line(ts: i64, event: &Event) -> Vec<u8> {
            format_entry(UnixTimestamp(ts), event.direction, &event.payload)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(128))]

            // Feature: remote-connection-manager, Property 42: For any interleaving of session I/O before and after logging is enabled, the log contains exactly the I/O produced after logging was enabled and none produced before.
            #[test]
            fn mid_session_enable_captures_only_post_enable_io(
                ts in -2_000_000_000i64..=4_000_000_000i64,
                pre in vec(event(), 0..12),
                post in vec(event(), 0..12),
            ) {
                let shared = InMemorySink::new();
                let logger = logger_with(ts, shared.clone());
                let session = SessionId::new();

                // Pre-enable I/O: no sink exists yet, so every record is dropped.
                for ev in &pre {
                    logger
                        .record(session, ev.direction, &ev.payload)
                        .expect("pre-enable record is a no-op");
                }
                prop_assert!(shared.entries().is_empty(), "pre-enable I/O was captured");

                // Enable logging mid-session, then replay the post-enable I/O.
                logger
                    .start(session, Path::new("ignored.log"))
                    .expect("start opens the in-memory sink");
                prop_assert!(logger.is_logging(session));
                for ev in &post {
                    logger
                        .record(session, ev.direction, &ev.payload)
                        .expect("post-enable record succeeds");
                }

                // The log holds exactly the post-enable events, in order, and
                // nothing from before the enable.
                let expected: Vec<Vec<u8>> =
                    post.iter().map(|ev| expected_line(ts, ev)).collect();
                prop_assert_eq!(shared.entries(), expected);
            }

            // Feature: remote-connection-manager, Property 43: For any logged content, disabling logging closes the file retaining all entries written up to that point and records nothing afterward.
            #[test]
            fn disabling_logging_preserves_written_entries(
                ts in -2_000_000_000i64..=4_000_000_000i64,
                before in vec(event(), 0..12),
                after in vec(event(), 0..12),
            ) {
                let shared = InMemorySink::new();
                let logger = logger_with(ts, shared.clone());
                let session = SessionId::new();

                logger
                    .start(session, Path::new("ignored.log"))
                    .expect("start opens the in-memory sink");
                for ev in &before {
                    logger
                        .record(session, ev.direction, &ev.payload)
                        .expect("record while logging succeeds");
                }

                // Snapshot everything written up to the stop.
                let retained_at_stop = shared.entries();

                logger.stop(session).expect("stop closes the sink");
                prop_assert!(!logger.is_logging(session), "still logging after stop");
                prop_assert!(shared.is_closed(), "sink was not closed on stop");

                // Post-disable I/O is a no-op; nothing is appended.
                for ev in &after {
                    logger
                        .record(session, ev.direction, &ev.payload)
                        .expect("post-stop record is a no-op");
                }

                let final_entries = shared.entries();
                // All pre-stop entries are retained, in order...
                let expected: Vec<Vec<u8>> =
                    before.iter().map(|ev| expected_line(ts, ev)).collect();
                prop_assert_eq!(&retained_at_stop, &expected);
                // ...and nothing was recorded after disabling.
                prop_assert_eq!(final_entries, retained_at_stop);
            }

            // Feature: remote-connection-manager, Property 45: For any log write that fails mid-stream, all entries written before the failure remain in the file and an error is reported.
            #[test]
            fn write_failure_preserves_prior_entries(
                ts in -2_000_000_000i64..=4_000_000_000i64,
                events in vec(event(), 1..16),
                // Fail somewhere within the stream (0-based index of the first
                // failing write).
                fail_at in 0usize..16usize,
            ) {
                let fail_at = fail_at % events.len();

                let shared = InMemorySink::failing_after(fail_at);
                let logger = logger_with(ts, shared.clone());
                let session = SessionId::new();
                logger
                    .start(session, Path::new("ignored.log"))
                    .expect("start opens the in-memory sink");

                let mut saw_error = false;
                for (index, ev) in events.iter().enumerate() {
                    let result = logger.record(session, ev.direction, &ev.payload);
                    if index < fail_at {
                        prop_assert!(result.is_ok(), "write before the failure point failed");
                    } else {
                        prop_assert!(
                            matches!(result, Err(LogError::WriteFailed(_))),
                            "expected WriteFailed at index {}",
                            index
                        );
                        saw_error = true;
                    }
                }
                prop_assert!(saw_error, "no write failure was reported");

                // Exactly the entries written before the failure point remain.
                let expected: Vec<Vec<u8>> = events[..fail_at]
                    .iter()
                    .map(|ev| expected_line(ts, ev))
                    .collect();
                prop_assert_eq!(shared.entries(), expected);
            }

            // Feature: remote-connection-manager, Property 46: For any configured log location that does not exist or is not writable at initiation, logging fails with an error and the Session is not opened with logging enabled.
            #[test]
            fn invalid_destination_prevents_logging_enabled_start(
                dest in "[a-zA-Z0-9_./-]{1,40}",
            ) {
                let logger = SessionLogger::new(FixedClock(0), RejectingFactory);
                let session = SessionId::new();

                let result = logger.start(session, Path::new(&dest));

                prop_assert!(
                    matches!(result, Err(LogError::InvalidDestination(_))),
                    "expected InvalidDestination for {:?}",
                    dest
                );
                // The session must not be left logging-enabled after a rejected
                // destination.
                prop_assert!(
                    !logger.is_logging(session),
                    "session is logging despite an invalid destination"
                );
            }
        }
    }
}
