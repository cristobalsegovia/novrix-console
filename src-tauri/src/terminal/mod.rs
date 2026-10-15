//! Terminal_Emulator: the ANSI state machine, screen grid, and scrollback.
//!
//! This module is the Rust core of the Terminal_Emulator component. It parses
//! the raw byte stream from a text-based Session through an ANSI/VT state
//! machine ([`vte`]), maintains a [`Grid`] of styled [`Cell`]s with a cursor,
//! keeps a bounded FIFO [`ScrollbackBuffer`] of scrolled-off lines, and reports
//! viewport resizes so the remote can be notified. It satisfies:
//!
//! - **Requirement 10.3 / Property 26** — interprets cursor positioning and SGR
//!   foreground/background color + bold/underline into the queryable grid.
//! - **Requirement 10.4 / Property 27** — discards unrecognized/malformed
//!   sequences and continues without corrupting state.
//! - **Requirement 10.5 / Property 28** — exposes resize and reports the new
//!   (cols, rows) for the remote.
//! - **Requirement 10.6 / Property 29** — scrollback is a bounded FIFO of
//!   >=1000 lines, oldest discarded first.
//!
//! The screen state is a plain, queryable reference model (no I/O), which is
//! what makes the ANSI behavior directly testable by the property tests in
//! tasks 14.2-14.5. Rendering to xterm.js is UI wiring handled separately
//! (task 21.x); this module produces the state the UI renders from.
//!
//! Cell/color/style/grid are terminal-specific types defined here rather than
//! in [`crate::domain`]; the shared [`DomainError`](crate::domain::DomainError)
//! is used where this module surfaces structured errors to the rest of the core.

mod color;
mod emulator;
mod grid;
mod scrollback;

#[cfg(test)]
mod property_tests;

pub use color::{AnsiColor, CellStyle, Color};
pub use emulator::{Dimensions, TerminalEmulator};
pub use grid::{Cell, Cursor, Grid};
pub use scrollback::{ScrollbackBuffer, DEFAULT_SCROLLBACK_LINES};
