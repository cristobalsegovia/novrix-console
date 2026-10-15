//! The write-destination port for the Session_Logger.
//!
//! [`LogSink`] abstracts "where log entries go" so the logger's control flow
//! (validate destination, write entries, retain-on-failure, close) can be
//! exercised without touching the real filesystem. Tests inject [`InMemorySink`]
//! (optionally armed to fail a write) to prove Property 42/43/45; production
//! uses [`FileSink`] created by [`FileSinkFactory`].
//!
//! A [`SinkFactory`] is the seam for destination validation: `open(dest)` must
//! fail with [`LogError::InvalidDestination`] when the destination does not
//! exist or is not writable, so the logger can refuse to start logging-enabled
//! (Requirement 14.6, Property 46).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::domain::LogError;

/// A destination that accepts timestamp-prefixed log entries.
///
/// `write_entry` appends one already-formatted entry (the logger owns framing
/// and the ISO-8601 prefix). A failure returns [`LogError::WriteFailed`]; the
/// contract is that any entry written *before* the failure is retained by the
/// sink (Requirement 14.5, Property 45). `close` flushes and releases the
/// destination, retaining everything written so far (Requirement 14.3).
pub trait LogSink: Send {
    /// Append one formatted entry. On failure, previously written entries are
    /// retained in the destination.
    fn write_entry(&mut self, entry: &[u8]) -> Result<(), LogError>;

    /// Flush and release the destination, retaining all entries written.
    fn close(&mut self) -> Result<(), LogError>;
}

/// Opens a [`LogSink`] for a destination path, validating it up front.
///
/// `open` performs the Requirement 14.6 check: it must reject a destination
/// whose parent directory is missing or not writable with
/// [`LogError::InvalidDestination`] *before* any Session is opened with logging
/// enabled.
pub trait SinkFactory: Send + Sync {
    /// The sink type this factory produces.
    type Sink: LogSink;

    /// Validate `dest` and open a sink, or fail with
    /// [`LogError::InvalidDestination`] if the location cannot be written.
    fn open(&self, dest: &Path) -> Result<Self::Sink, LogError>;
}

// --- File-backed production sink -----------------------------------------

/// Opens real files on disk, creating/truncating the log file for a session.
#[derive(Debug, Default, Clone, Copy)]
pub struct FileSinkFactory;

impl SinkFactory for FileSinkFactory {
    type Sink = FileSink;

    fn open(&self, dest: &Path) -> Result<FileSink, LogError> {
        // Requirement 14.6 / Property 46: the destination must be writable at
        // initiation. Require the parent directory to already exist (the logger
        // writes into an existing location rather than creating a tree) and
        // treat any failure to open for writing as an invalid destination.
        if let Some(parent) = dest.parent() {
            // An empty parent means `dest` is a bare filename in the CWD, which
            // is a valid relative destination; only a named, missing parent is
            // rejected here.
            if !parent.as_os_str().is_empty() && !parent.is_dir() {
                return Err(LogError::InvalidDestination(format!(
                    "log directory does not exist: {}",
                    parent.display()
                )));
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dest)
            .map_err(|e| {
                LogError::InvalidDestination(format!(
                    "cannot open log file {}: {e}",
                    dest.display()
                ))
            })?;

        Ok(FileSink { file: Some(file) })
    }
}

/// A [`LogSink`] backed by an open file handle.
#[derive(Debug)]
pub struct FileSink {
    file: Option<File>,
}

impl LogSink for FileSink {
    fn write_entry(&mut self, entry: &[u8]) -> Result<(), LogError> {
        match self.file.as_mut() {
            Some(file) => file
                .write_all(entry)
                .map_err(|e| LogError::WriteFailed(e.to_string())),
            None => Err(LogError::WriteFailed("log file is closed".to_string())),
        }
    }

    fn close(&mut self) -> Result<(), LogError> {
        if let Some(mut file) = self.file.take() {
            // Flush retains everything written; dropping the handle releases it.
            file.flush()
                .map_err(|e| LogError::WriteFailed(e.to_string()))?;
        }
        Ok(())
    }
}

// --- In-memory test sink --------------------------------------------------

/// An in-memory [`LogSink`] for unit tests.
///
/// It accumulates every successfully written entry so a test can assert exactly
/// what was captured (Property 42/43). `fail_after` arms it to fail the Nth
/// write (0-based) with [`LogError::WriteFailed`] while retaining all earlier
/// entries, exercising Property 45 without disk I/O. The buffer is shared via
/// `Arc<Mutex<...>>` so a test can keep inspecting entries after the sink is
/// moved into the logger.
#[derive(Debug, Clone, Default)]
pub struct InMemorySink {
    entries: Arc<Mutex<Vec<Vec<u8>>>>,
    writes_seen: Arc<Mutex<usize>>,
    fail_after: Option<usize>,
    closed: Arc<Mutex<bool>>,
}

impl InMemorySink {
    /// A sink that accepts every write.
    pub fn new() -> Self {
        Self::default()
    }

    /// A sink that fails the write at index `n` (0-based), retaining earlier
    /// entries. Writes after a failure also fail.
    pub fn failing_after(n: usize) -> Self {
        Self {
            fail_after: Some(n),
            ..Self::default()
        }
    }

    /// A shared handle to the captured entries, for post-hoc assertions.
    pub fn entries_handle(&self) -> Arc<Mutex<Vec<Vec<u8>>>> {
        Arc::clone(&self.entries)
    }

    /// Snapshot of the entries captured so far.
    pub fn entries(&self) -> Vec<Vec<u8>> {
        self.entries.lock().unwrap().clone()
    }

    /// Whether `close` has been called.
    pub fn is_closed(&self) -> bool {
        *self.closed.lock().unwrap()
    }
}

impl LogSink for InMemorySink {
    fn write_entry(&mut self, entry: &[u8]) -> Result<(), LogError> {
        let mut seen = self.writes_seen.lock().unwrap();
        let index = *seen;
        *seen += 1;

        if let Some(fail_at) = self.fail_after {
            if index >= fail_at {
                // Prior entries remain in `self.entries`; nothing is removed.
                return Err(LogError::WriteFailed(format!(
                    "simulated write failure at entry {index}"
                )));
            }
        }

        self.entries.lock().unwrap().push(entry.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<(), LogError> {
        *self.closed.lock().unwrap() = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_sink_captures_entries() {
        let mut sink = InMemorySink::new();
        sink.write_entry(b"a").unwrap();
        sink.write_entry(b"b").unwrap();
        assert_eq!(sink.entries(), vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn failing_sink_retains_prior_entries() {
        let mut sink = InMemorySink::failing_after(1);
        sink.write_entry(b"first").unwrap();
        let err = sink.write_entry(b"second").unwrap_err();
        assert!(matches!(err, LogError::WriteFailed(_)));
        // The first (pre-failure) entry is retained; the failed one is not.
        assert_eq!(sink.entries(), vec![b"first".to_vec()]);
    }

    #[test]
    fn file_factory_rejects_missing_directory() {
        // Build a path whose *parent directory* provably does not exist, in an
        // OS-portable way: a unique, non-existent subdirectory under the real
        // temp dir. A hard-coded Unix path like "/this/does/not/exist" is not
        // portable — on Windows a leading "/" resolves against the current
        // drive and may behave differently — so anchor to `temp_dir()` instead.
        let factory = FileSinkFactory;
        let missing = std::env::temp_dir()
            .join("novrix-nonexistent-dir-9f3a1c7b")
            .join("session.log");
        let err = factory.open(&missing).unwrap_err();
        assert!(matches!(err, LogError::InvalidDestination(_)));
    }
}
