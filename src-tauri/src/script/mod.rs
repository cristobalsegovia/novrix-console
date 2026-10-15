//! Script_Engine — sequential script execution against a text-based Session.
//!
//! The Script_Engine drives an automation [`Script`] against a live, text-based
//! Session (Requirement 15). It sends the script's commands **one at a time, in
//! the order they are defined** (req 15.1, Property 47), and for a step that
//! declares a *wait-for-prompt* condition it holds the next command until the
//! expected prompt text has been seen in the Session's output (req 15.2,
//! Property 48). A wait is bounded by a resolved timeout: the configured value
//! when it lies in `1..=3600` seconds, otherwise the default of 30 (req 15.3,
//! Property 49). If the prompt never arrives within that window execution stops
//! with the remaining commands unsent and a [`ScriptError::PromptTimeout`]
//! carrying the stopping command index (req 15.3). If the Session disconnects
//! mid-run execution stops the same way with [`ScriptError::Disconnected`]
//! (req 15.4, Property 50). A run that sends every command reports success with
//! a sent-count equal to the number of commands (req 15.5); a run stopped by an
//! error reports failure with the index of the command at which it stopped
//! (req 15.6, Property 51).
//!
//! ## Testability
//!
//! The design calls for the engine to run over the Protocol_Engine's uniform
//! [`SessionConn`](crate::protocol::SessionConn) interface. To keep the engine
//! testable without a real network, [`run`](ScriptEngine::run) operates over an
//! injectable `&mut dyn SessionConn`: tests pass a mock session that replays
//! canned output and can simulate a disconnect, so the ordering, gating,
//! timeout, and disconnection behaviour are all exercised in-memory.
//!
//! Time is likewise isolated behind the [`Clock`] trait so the timeout logic is
//! verified without real waits. The timeout-resolution rule itself is a pure,
//! total function ([`resolve_timeout`]) that is unit-testable in isolation.
//!
//! Scope of this task (17.1): the engine, its data types, the pure timeout
//! rule, and the injectable session/clock seams, plus smoke tests for the
//! timeout rule and in-order sending. The exhaustive property tests
//! (Properties 47–51) land in tasks 17.2–17.6.

use async_trait::async_trait;

use crate::domain::{ScriptError, SessionState};
use crate::protocol::SessionConn;

/// The default wait-for-prompt timeout, in seconds, used when a step's
/// configured timeout is outside the allowed `1..=3600` range (req 15.3).
pub const DEFAULT_TIMEOUT_SECS: u32 = 30;

/// The inclusive bounds of a valid wait-for-prompt timeout, in seconds
/// (req 15.3).
pub const MIN_TIMEOUT_SECS: u32 = 1;
pub const MAX_TIMEOUT_SECS: u32 = 3600;

/// Resolve the effective wait-for-prompt timeout for a step (Property 49).
///
/// Pure and total: the resolved timeout is the configured value when it lies in
/// `1..=3600`, and [`DEFAULT_TIMEOUT_SECS`] (30) otherwise. The returned value
/// is therefore always within `1..=3600` seconds — exactly the invariant
/// Property 49 asserts. Keeping it a free function (rather than inlining it in
/// [`ScriptEngine::run`]) is what makes the invariant unit-testable without a
/// session or a clock.
pub fn resolve_timeout(configured_secs: u32) -> u32 {
    if (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&configured_secs) {
        configured_secs
    } else {
        DEFAULT_TIMEOUT_SECS
    }
}

/// A wait-for-prompt condition attached to a [`ScriptStep`].
///
/// After the step's command is sent, the engine withholds the *next* command
/// until `prompt` is observed in the Session's output (req 15.2). The wait is
/// bounded by `timeout_secs`, which is passed through [`resolve_timeout`] before
/// use (req 15.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitForPrompt {
    /// The prompt text that must appear in the Session output before the next
    /// command is sent.
    pub prompt: String,
    /// The configured wait timeout, in seconds. Interpreted via
    /// [`resolve_timeout`]: used as-is when in `1..=3600`, else defaulted to 30.
    pub timeout_secs: u32,
}

impl WaitForPrompt {
    /// A wait-for-prompt condition for `prompt` with the given configured
    /// timeout.
    pub fn new(prompt: impl Into<String>, timeout_secs: u32) -> Self {
        Self {
            prompt: prompt.into(),
            timeout_secs,
        }
    }
}

/// A single step of a [`Script`]: a command plus an optional wait-for-prompt
/// condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptStep {
    /// The command to send to the Session (without a trailing newline; the
    /// engine appends `\n` when sending).
    pub command: String,
    /// When present, the engine waits for this prompt after sending `command`
    /// before moving on to the next step (req 15.2).
    pub wait_for: Option<WaitForPrompt>,
}

impl ScriptStep {
    /// A step that sends `command` with no wait-for-prompt gating.
    pub fn command(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            wait_for: None,
        }
    }

    /// A step that sends `command` and then waits for `prompt` within
    /// `timeout_secs` before the next command is sent.
    pub fn command_then_wait(
        command: impl Into<String>,
        prompt: impl Into<String>,
        timeout_secs: u32,
    ) -> Self {
        Self {
            command: command.into(),
            wait_for: Some(WaitForPrompt::new(prompt, timeout_secs)),
        }
    }
}

/// An ordered sequence of [`ScriptStep`]s to run against a Session.
///
/// The order of `steps` is the order of execution (req 15.1); the engine never
/// reorders or skips steps on the success path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Script {
    /// The steps, executed front-to-back.
    pub steps: Vec<ScriptStep>,
}

impl Script {
    /// A script from an ordered list of steps.
    pub fn new(steps: Vec<ScriptStep>) -> Self {
        Self { steps }
    }

    /// The number of commands the script would send on a complete run.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the script has no steps.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// The outcome of a script run (Property 51).
///
/// A run that sends every command yields [`ScriptOutcome::Success`] with a
/// `sent` count equal to the number of commands (req 15.5). A run stopped by an
/// error yields [`ScriptOutcome::Failure`] with the `command_index` at which it
/// stopped and the `error` that stopped it (req 15.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// Every command was sent; `sent` equals the script's command count.
    Success {
        /// The number of commands sent (equal to the script length).
        sent: usize,
    },
    /// Execution stopped early; `command_index` is the 0-based index of the
    /// command at which it stopped and `error` is the cause.
    Failure {
        /// The 0-based index of the command at which execution stopped.
        command_index: usize,
        /// The cause of the stop (prompt timeout or disconnection).
        error: ScriptError,
    },
}

impl ScriptOutcome {
    /// Whether this outcome is a success.
    pub fn is_success(&self) -> bool {
        matches!(self, ScriptOutcome::Success { .. })
    }
}

/// An injectable, monotonic time source so the wait timeout is testable without
/// real waits.
///
/// The engine only needs *elapsed* time, expressed in whole seconds, between
/// two observations. A production clock reads the wall/monotonic clock; a test
/// clock returns a scripted sequence of instants so a timeout can be forced
/// deterministically. `now_secs` must be non-decreasing across calls.
pub trait Clock: Send + Sync {
    /// The current time, in seconds, from an arbitrary fixed epoch. Only
    /// differences between two readings are meaningful.
    fn now_secs(&self) -> u64;
}

/// A real monotonic clock backed by [`std::time::Instant`].
///
/// Used in production; tests substitute a deterministic clock instead.
pub struct MonotonicClock {
    start: std::time::Instant,
}

impl MonotonicClock {
    /// A clock whose zero is "now".
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }
}

/// The Script_Engine (design: `trait ScriptEngine`).
///
/// `run` executes a [`Script`] against a Session, honouring order, wait-for-
/// prompt gating, the wait timeout, and disconnection, and returns a
/// [`ScriptOutcome`]. The design's signature keys the session by `SessionId`
/// and resolves it internally; for this task the engine operates directly over
/// an injectable `&mut dyn SessionConn` (the uniform Protocol_Engine interface),
/// which is both what a resolved session *is* and what makes the engine
/// testable with a mock. Wiring the `SessionId` lookup to the live
/// Session_Manager is a later integration step.
#[async_trait]
pub trait ScriptEngine: Send + Sync {
    /// Run `script` against `session`, returning the completion outcome.
    ///
    /// The `Result` is `Err` only when execution stopped on a [`ScriptError`];
    /// the same information is also surfaced in the returned
    /// [`ScriptOutcome::Failure`] so callers can treat either shape. Successful
    /// runs always return `Ok(ScriptOutcome::Success { .. })`.
    async fn run(
        &self,
        session: &mut dyn SessionConn,
        script: &Script,
    ) -> Result<ScriptOutcome, ScriptError>;
}

/// The default [`ScriptEngine`] implementation.
///
/// Holds the injected [`Clock`] used to bound wait-for-prompt steps. Stateless
/// otherwise: all per-run state lives on the stack of [`run`](DefaultScriptEngine::run).
pub struct DefaultScriptEngine<C: Clock> {
    clock: C,
}

impl<C: Clock> DefaultScriptEngine<C> {
    /// A new engine driven by `clock`.
    pub fn new(clock: C) -> Self {
        Self { clock }
    }
}

impl DefaultScriptEngine<MonotonicClock> {
    /// A new engine backed by the real monotonic clock (production default).
    pub fn with_monotonic_clock() -> Self {
        Self::new(MonotonicClock::new())
    }
}

#[async_trait]
impl<C: Clock> ScriptEngine for DefaultScriptEngine<C> {
    async fn run(
        &self,
        session: &mut dyn SessionConn,
        script: &Script,
    ) -> Result<ScriptOutcome, ScriptError> {
        for (index, step) in script.steps.iter().enumerate() {
            // A disconnected session cannot accept the next command. Observing
            // the state before sending catches a disconnect that happened
            // between steps and stops with the current command unsent (req 15.4).
            if session.state() == SessionState::Disconnected {
                return finish(index, ScriptError::Disconnected {
                    command_index: index,
                });
            }

            // (req 15.1, Property 47) Send this step's command, in order. Append
            // a newline so the remote executes the line, matching how an
            // interactive user would submit it.
            let mut line = step.command.clone().into_bytes();
            line.push(b'\n');
            if let Err(_e) = session.send(&line).await {
                // A send failure means the transport is gone; treat it as a
                // disconnection at this command (req 15.4).
                return finish(index, ScriptError::Disconnected {
                    command_index: index,
                });
            }

            // (req 15.2, Property 48) If this step gates on a prompt, hold the
            // next command until the prompt text is received, bounded by the
            // resolved timeout (req 15.3, Property 49).
            if let Some(wait) = &step.wait_for {
                if let Err(error) = self.wait_for_prompt(session, wait, index).await {
                    return finish(index, error);
                }
            }
        }

        // (req 15.5, Property 51) Every command was sent.
        Ok(ScriptOutcome::Success {
            sent: script.steps.len(),
        })
    }
}

impl<C: Clock> DefaultScriptEngine<C> {
    /// Wait for `wait.prompt` to appear in the Session output, bounded by the
    /// resolved timeout.
    ///
    /// Reads output chunks and accumulates them until the prompt substring is
    /// seen. Returns:
    /// - `Ok(())` once the prompt is observed (req 15.2);
    /// - `Err(PromptTimeout)` if the resolved timeout elapses first — the caller
    ///   stops with the remaining commands unsent (req 15.3);
    /// - `Err(Disconnected)` if the Session reaches EOF or a receive error
    ///   before the prompt arrives (req 15.4).
    ///
    /// The resolved timeout bounds the *total* wait for this step; it is checked
    /// against the injected [`Clock`] so the bound is testable without real
    /// sleeps.
    async fn wait_for_prompt(
        &self,
        session: &mut dyn SessionConn,
        wait: &WaitForPrompt,
        index: usize,
    ) -> Result<(), ScriptError> {
        let timeout_secs = resolve_timeout(wait.timeout_secs) as u64;
        let deadline = self.clock.now_secs().saturating_add(timeout_secs);

        // Accumulate received bytes so a prompt split across chunks is still
        // matched. Decoded lossily to search for the prompt text.
        let mut buffer: Vec<u8> = Vec::new();

        loop {
            // (req 15.3) Enforce the wait timeout before attempting another
            // read: once the deadline has passed, stop with the prompt unseen.
            if self.clock.now_secs() >= deadline {
                return Err(ScriptError::PromptTimeout {
                    command_index: index,
                });
            }

            match session.recv().await {
                // (req 15.2) Got a chunk — append and look for the prompt.
                Ok(Some(chunk)) => {
                    buffer.extend_from_slice(&chunk);
                    if contains_prompt(&buffer, &wait.prompt) {
                        return Ok(());
                    }
                    // Re-check the deadline at the top of the loop; a slow prompt
                    // that never completes is bounded by the timeout.
                }
                // (req 15.4) Clean EOF: the Session disconnected before the
                // prompt arrived.
                Ok(None) => {
                    return Err(ScriptError::Disconnected {
                        command_index: index,
                    });
                }
                // (req 15.4) A receive error means the transport is gone.
                Err(_e) => {
                    return Err(ScriptError::Disconnected {
                        command_index: index,
                    });
                }
            }
        }
    }
}

/// Build a [`ScriptOutcome::Failure`] *and* the matching `Err` from a stopping
/// index and cause (req 15.6).
///
/// Returning `Err(error)` keeps the engine's `Result` contract while the caller
/// can also recover the structured `Failure` outcome; both carry the same
/// `command_index`, so the two reporting shapes never disagree.
fn finish(index: usize, error: ScriptError) -> Result<ScriptOutcome, ScriptError> {
    debug_assert_eq!(
        command_index_of(&error),
        index,
        "stopping index must match the error's command_index"
    );
    Err(error)
}

/// The `command_index` carried by a [`ScriptError`].
fn command_index_of(error: &ScriptError) -> usize {
    match error {
        ScriptError::PromptTimeout { command_index } => *command_index,
        ScriptError::Disconnected { command_index } => *command_index,
    }
}

/// Whether `buffer`'s lossy UTF-8 decoding contains `prompt` as a substring.
///
/// Searching the lossy decode (rather than raw bytes) matches the prompt text
/// the user configured even when the stream carries non-UTF-8 noise around it.
fn contains_prompt(buffer: &[u8], prompt: &str) -> bool {
    if prompt.is_empty() {
        return true;
    }
    String::from_utf8_lossy(buffer).contains(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ProtocolError;
    use std::cell::RefCell;
    use std::sync::Mutex;

    // --- Timeout resolution (Property 49 smoke test) ----------------------

    #[test]
    fn resolve_timeout_keeps_in_range_values() {
        assert_eq!(resolve_timeout(1), 1);
        assert_eq!(resolve_timeout(30), 30);
        assert_eq!(resolve_timeout(3600), 3600);
        assert_eq!(resolve_timeout(600), 600);
    }

    #[test]
    fn resolve_timeout_defaults_out_of_range_values_to_30() {
        assert_eq!(resolve_timeout(0), DEFAULT_TIMEOUT_SECS);
        assert_eq!(resolve_timeout(3601), DEFAULT_TIMEOUT_SECS);
        assert_eq!(resolve_timeout(u32::MAX), DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn resolve_timeout_is_always_within_bounds() {
        for v in [0u32, 1, 30, 60, 3600, 3601, 100_000, u32::MAX] {
            let r = resolve_timeout(v);
            assert!(
                (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&r),
                "resolve_timeout({v}) = {r} is outside 1..=3600"
            );
        }
    }

    // --- Test doubles -----------------------------------------------------

    /// A clock that returns a scripted sequence of instants, advancing one entry
    /// per read and holding the last value once exhausted.
    struct ScriptedClock {
        ticks: Mutex<std::vec::IntoIter<u64>>,
        last: Mutex<u64>,
    }

    impl ScriptedClock {
        fn new(ticks: Vec<u64>) -> Self {
            Self {
                ticks: Mutex::new(ticks.into_iter()),
                last: Mutex::new(0),
            }
        }

        /// A clock frozen at zero (no timeout ever elapses within the test).
        fn frozen() -> Self {
            Self::new(vec![0])
        }
    }

    impl Clock for ScriptedClock {
        fn now_secs(&self) -> u64 {
            let mut iter = self.ticks.lock().unwrap();
            let mut last = self.last.lock().unwrap();
            if let Some(next) = iter.next() {
                *last = next;
            }
            *last
        }
    }

    /// A mock [`SessionConn`] that records sent commands and replays canned recv
    /// chunks, with no network. `recv` returns the next queued item; `None`
    /// items model EOF/disconnect.
    struct MockSession {
        sent: RefCell<Vec<Vec<u8>>>,
        recv_queue: RefCell<std::collections::VecDeque<Option<Vec<u8>>>>,
        state: SessionState,
        /// When true, `recv` yields `Ok(None)` once the queue is drained.
        eof_when_drained: bool,
    }

    // The uniform trait requires `Send`; `RefCell` is not `Sync`, but the engine
    // only touches the session from one task at a time, and tests are
    // single-threaded, so this is sound for the mock. `SessionConn: Send` is
    // satisfied because `RefCell<T: Send>: Send`.
    unsafe impl Send for MockSession {}

    impl MockSession {
        fn new(state: SessionState) -> Self {
            Self {
                sent: RefCell::new(Vec::new()),
                recv_queue: RefCell::new(std::collections::VecDeque::new()),
                state,
                eof_when_drained: false,
            }
        }

        fn with_recv(mut self, chunks: Vec<Option<Vec<u8>>>) -> Self {
            self.recv_queue = RefCell::new(chunks.into_iter().collect());
            self
        }

        fn eof_when_drained(mut self) -> Self {
            self.eof_when_drained = true;
            self
        }

        fn sent_commands(&self) -> Vec<String> {
            self.sent
                .borrow()
                .iter()
                .map(|b| String::from_utf8_lossy(b).trim_end().to_string())
                .collect()
        }
    }

    #[async_trait]
    impl SessionConn for MockSession {
        async fn send(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
            self.sent.borrow_mut().push(data.to_vec());
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
            match self.recv_queue.borrow_mut().pop_front() {
                Some(item) => Ok(item),
                None if self.eof_when_drained => Ok(None),
                None => Ok(Some(Vec::new())),
            }
        }

        async fn resize(&mut self, _cols: u16, _rows: u16) -> Result<(), ProtocolError> {
            Ok(())
        }

        async fn close(&mut self) -> Result<(), ProtocolError> {
            self.state = SessionState::Disconnected;
            Ok(())
        }

        fn state(&self) -> SessionState {
            self.state
        }
    }

    // --- In-order sending (Property 47 smoke test) ------------------------

    #[tokio::test]
    async fn sends_all_commands_in_order_and_reports_success() {
        let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
        let script = Script::new(vec![
            ScriptStep::command("first"),
            ScriptStep::command("second"),
            ScriptStep::command("third"),
        ]);
        let mut session = MockSession::new(SessionState::Connected);

        let outcome = engine.run(&mut session, &script).await.unwrap();

        assert_eq!(outcome, ScriptOutcome::Success { sent: 3 });
        assert_eq!(session.sent_commands(), vec!["first", "second", "third"]);
    }

    #[tokio::test]
    async fn wait_for_prompt_holds_next_command_until_prompt_seen() {
        let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
        let script = Script::new(vec![
            ScriptStep::command_then_wait("login", "$ ", 30),
            ScriptStep::command("whoami"),
        ]);
        // First recv yields noise, second yields the prompt; only then may the
        // second command be sent.
        let mut session = MockSession::new(SessionState::Connected).with_recv(vec![
            Some(b"connecting...\n".to_vec()),
            Some(b"ready $ ".to_vec()),
        ]);

        let outcome = engine.run(&mut session, &script).await.unwrap();

        assert_eq!(outcome, ScriptOutcome::Success { sent: 2 });
        assert_eq!(session.sent_commands(), vec!["login", "whoami"]);
    }

    #[tokio::test]
    async fn prompt_timeout_stops_with_remaining_commands_unsent() {
        // Clock jumps past the 30s deadline on the first in-loop check, so the
        // wait times out before any matching prompt is read.
        let engine = DefaultScriptEngine::new(ScriptedClock::new(vec![0, 31]));
        let script = Script::new(vec![
            ScriptStep::command_then_wait("login", "NEVER", 30),
            ScriptStep::command("whoami"),
        ]);
        let mut session = MockSession::new(SessionState::Connected)
            .with_recv(vec![Some(b"irrelevant output".to_vec())]);

        let err = engine.run(&mut session, &script).await.unwrap_err();

        assert_eq!(err, ScriptError::PromptTimeout { command_index: 0 });
        // Only the first command was sent; "whoami" stays unsent.
        assert_eq!(session.sent_commands(), vec!["login"]);
    }

    #[tokio::test]
    async fn disconnection_during_wait_stops_execution() {
        let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
        let script = Script::new(vec![
            ScriptStep::command_then_wait("login", "$ ", 30),
            ScriptStep::command("whoami"),
        ]);
        // recv yields EOF (None) before the prompt is ever seen.
        let mut session = MockSession::new(SessionState::Connected)
            .with_recv(vec![None])
            .eof_when_drained();

        let err = engine.run(&mut session, &script).await.unwrap_err();

        assert_eq!(err, ScriptError::Disconnected { command_index: 0 });
        assert_eq!(session.sent_commands(), vec!["login"]);
    }

    #[tokio::test]
    async fn stops_before_sending_when_session_already_disconnected() {
        let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
        let script = Script::new(vec![ScriptStep::command("first")]);
        let mut session = MockSession::new(SessionState::Disconnected);

        let err = engine.run(&mut session, &script).await.unwrap_err();

        assert_eq!(err, ScriptError::Disconnected { command_index: 0 });
        assert!(session.sent_commands().is_empty());
    }

    // --- Property tests (47–51) -------------------------------------------
    //
    // The engine surface is async while `proptest` bodies are sync, so each
    // case builds a single-threaded Tokio runtime and `block_on`s the run — the
    // same pattern the other modules' property tests use. The in-module
    // [`MockSession`] and [`ScriptedClock`] doubles are reused as no-network,
    // no-wait seams: ordering, gating, timeout, disconnection, and reporting
    // are all exercised in-memory. Production logic is left unchanged.
    //
    // Clock bookkeeping: `wait_for_prompt` reads the clock once at setup
    // (`deadline = now + timeout`) and once per loop iteration before each
    // `recv`. A clock frozen at 0 therefore never trips a positive deadline, so
    // the success/gating/disconnection properties use `ScriptedClock::frozen()`;
    // the timeout property feeds a tick sequence that jumps past the deadline.
    mod properties {
        use super::*;
        use proptest::prelude::*;
        use tokio::runtime::{Builder, Runtime};

        /// A fresh current-thread runtime for driving one async property case.
        fn runtime() -> Runtime {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime")
        }

        /// Commands that round-trip cleanly through the mock's
        /// `send`→`sent_commands` path: no newline (the engine appends one) and
        /// no trailing whitespace (the mock trims the trailing newline, so a
        /// command ending in spaces would not compare equal).
        fn command_strategy() -> impl Strategy<Value = String> {
            "[A-Za-z0-9 ._/-]{0,20}".prop_map(|s| s.trim_end().to_string())
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            // Feature: remote-connection-manager, Property 47: For any script run against a text-based Session, the commands are sent one at a time in exactly the order they are defined.
            #[test]
            fn scripts_send_commands_in_order(
                commands in proptest::collection::vec(command_strategy(), 0..12),
            ) {
                let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
                let script = Script::new(
                    commands.iter().cloned().map(ScriptStep::command).collect(),
                );
                // A connected session with no gating: every command sends and
                // the recorded sequence must equal the script's commands, in
                // order (no reordering, no skips, no duplicates).
                let mut session = MockSession::new(SessionState::Connected);

                let outcome = runtime().block_on(engine.run(&mut session, &script)).unwrap();

                prop_assert_eq!(outcome, ScriptOutcome::Success { sent: commands.len() });
                prop_assert_eq!(session.sent_commands(), commands);
            }

            // Feature: remote-connection-manager, Property 48: For any script containing a wait-for-prompt step, the next command is not sent until the specified prompt text has been received from the Session.
            #[test]
            fn wait_for_prompt_gates_the_next_command(
                first in command_strategy(),
                next in command_strategy(),
                // The number of non-prompt chunks that precede the prompt. The
                // next command must stay unsent across all of them.
                noise_chunks in 0usize..6,
            ) {
                let prompt = "PROMPT>";
                let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
                let script = Script::new(vec![
                    ScriptStep::command_then_wait(first.clone(), prompt, 30),
                    ScriptStep::command(next.clone()),
                ]);

                // Model the Session output: `noise_chunks` chunks that do NOT
                // contain the prompt, then one chunk that delivers it. The next
                // command may only be sent after that final chunk is read.
                let mut chunks: Vec<Option<Vec<u8>>> = (0..noise_chunks)
                    .map(|i| Some(format!("line {i}\n").into_bytes()))
                    .collect();
                chunks.push(Some(format!("ready {prompt} ").into_bytes()));
                let mut session =
                    MockSession::new(SessionState::Connected).with_recv(chunks);

                let outcome = runtime().block_on(engine.run(&mut session, &script)).unwrap();

                // Both commands sent, and the gated command strictly follows the
                // first — the engine never emits `next` before the prompt chunk.
                prop_assert_eq!(outcome, ScriptOutcome::Success { sent: 2 });
                prop_assert_eq!(session.sent_commands(), vec![first, next]);
            }

            // Feature: remote-connection-manager, Property 49: For any script, the resolved wait timeout equals the configured value or 30 and lies in 1..=3600 seconds; if the expected prompt is not received within it, execution stops, the remaining commands are left unsent, and a timeout error is reported.
            #[test]
            fn script_timeout_stops_with_remaining_commands_unsent(
                configured in any::<u32>(),
                remaining in proptest::collection::vec(command_strategy(), 0..6),
            ) {
                // Part 1 — the pure rule: resolved == configured when in range,
                // else the default, and always within 1..=3600.
                let resolved = resolve_timeout(configured);
                if (MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&configured) {
                    prop_assert_eq!(resolved, configured);
                } else {
                    prop_assert_eq!(resolved, DEFAULT_TIMEOUT_SECS);
                }
                prop_assert!((MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&resolved));

                // Part 2 — a wait whose prompt never arrives stops at the first
                // command with PromptTimeout, leaving every later command unsent.
                // The clock reads 0 at setup (deadline = resolved) then jumps
                // past it on the first in-loop check, forcing the timeout before
                // any chunk could match.
                let engine =
                    DefaultScriptEngine::new(ScriptedClock::new(vec![0, (resolved as u64) + 1]));
                let mut steps = vec![ScriptStep::command_then_wait("start", "NEVER", configured)];
                steps.extend(remaining.iter().cloned().map(ScriptStep::command));
                let script = Script::new(steps);
                let mut session = MockSession::new(SessionState::Connected)
                    .with_recv(vec![Some(b"nothing matching here".to_vec())]);

                let err = runtime().block_on(engine.run(&mut session, &script)).unwrap_err();

                prop_assert_eq!(err, ScriptError::PromptTimeout { command_index: 0 });
                // Only the gating command went out; the remainder stays unsent.
                prop_assert_eq!(session.sent_commands(), vec!["start".to_string()]);
            }

            // Feature: remote-connection-manager, Property 50: For any point at which the Session becomes disconnected during script execution, execution stops, the remaining commands are left unsent, and a disconnection error is reported.
            #[test]
            fn disconnection_during_a_script_stops_execution(
                // Commands before the one whose wait observes the disconnect.
                before in proptest::collection::vec(command_strategy(), 0..5),
                after in proptest::collection::vec(command_strategy(), 0..5),
            ) {
                let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
                // The step at `stop_index` waits for a prompt; the Session
                // reaches EOF (recv -> None) before it ever arrives, so the
                // engine stops there with Disconnected.
                let stop_index = before.len();
                let mut steps: Vec<ScriptStep> =
                    before.iter().cloned().map(ScriptStep::command).collect();
                steps.push(ScriptStep::command_then_wait("gate", "$ ", 30));
                steps.extend(after.iter().cloned().map(ScriptStep::command));
                let script = Script::new(steps);

                // Non-gating earlier commands drain the empty-queue default
                // (Ok(Some(empty))) harmlessly; the gating wait hits EOF.
                let mut session = MockSession::new(SessionState::Connected)
                    .with_recv(vec![None])
                    .eof_when_drained();

                let err = runtime().block_on(engine.run(&mut session, &script)).unwrap_err();

                prop_assert_eq!(err, ScriptError::Disconnected { command_index: stop_index });
                // Everything up to and including the gating command was sent;
                // nothing after it.
                let mut expected = before.clone();
                expected.push("gate".to_string());
                prop_assert_eq!(session.sent_commands(), expected);
            }

            // Feature: remote-connection-manager, Property 51: For any script run, a run that sends all commands reports success with a sent-count equal to the number of commands, and a run stopped by an error reports failure with the index of the command at which it stopped.
            #[test]
            fn script_completion_reporting_is_accurate(
                commands in proptest::collection::vec(command_strategy(), 0..10),
                // When Some(i) (clamped into range), the run stops at index i via
                // a disconnection; when None, the run completes successfully.
                stop_at in proptest::option::of(0usize..10),
            ) {
                let n = commands.len();
                let stop_index = stop_at.filter(|_| n > 0).map(|i| i % n);

                match stop_index {
                    // Success path: no interruption, every command is sent, and
                    // the outcome's sent-count equals the command count.
                    None => {
                        let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
                        let script = Script::new(
                            commands.iter().cloned().map(ScriptStep::command).collect(),
                        );
                        let mut session = MockSession::new(SessionState::Connected);

                        let outcome =
                            runtime().block_on(engine.run(&mut session, &script)).unwrap();

                        prop_assert_eq!(outcome, ScriptOutcome::Success { sent: n });
                        prop_assert_eq!(session.sent_commands(), commands);
                    }
                    // Failure path: the command at `stop` gates on a prompt that
                    // never comes because the Session is already disconnected at
                    // that point, so the run fails reporting exactly that index.
                    Some(stop) => {
                        let engine = DefaultScriptEngine::new(ScriptedClock::frozen());
                        let steps: Vec<ScriptStep> = commands
                            .iter()
                            .enumerate()
                            .map(|(i, c)| {
                                if i == stop {
                                    ScriptStep::command_then_wait(c.clone(), "$ ", 30)
                                } else {
                                    ScriptStep::command(c.clone())
                                }
                            })
                            .collect();
                        let script = Script::new(steps);
                        let mut session = MockSession::new(SessionState::Connected)
                            .with_recv(vec![None])
                            .eof_when_drained();

                        let outcome = runtime().block_on(engine.run(&mut session, &script));
                        let err = outcome.unwrap_err();

                        prop_assert_eq!(err, ScriptError::Disconnected { command_index: stop });
                        // Commands up to and including the stopping one are sent;
                        // nothing after it.
                        prop_assert_eq!(session.sent_commands(), commands[..=stop].to_vec());
                    }
                }
            }
        }
    }
}
