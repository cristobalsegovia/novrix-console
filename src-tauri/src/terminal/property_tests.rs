//! Property-based tests for the Terminal_Emulator (tasks 14.2-14.5).
//!
//! These exercise the ANSI state machine, resize reporting, and the bounded
//! scrollback against the behavioral properties the design calls out
//! (Properties 26-29, validating Requirements 10.3-10.6). They use `proptest`
//! with smart generators constrained to the subset of ANSI the emulator
//! supports, and assert the emulator's queryable state matches an independent
//! reference model computed in the test.
//!
//! No production logic lives here; this module is `#[cfg(test)]` only.

use proptest::prelude::*;

use super::color::{AnsiColor, CellStyle, Color};
use super::grid::{Cell, Cursor};
use super::scrollback::{ScrollbackBuffer, DEFAULT_SCROLLBACK_LINES};
use super::emulator::{Dimensions, TerminalEmulator};

// Run a healthy number of cases per property (requirement: >= 100 iterations).
const CASES: u32 = 256;

// --- Property 26 reference model -------------------------------------------

/// Fixed viewport the Property-26 generator targets. Small enough that the
/// reference model is cheap, large enough to exercise 2-D cursor motion.
const COLS: u16 = 12;
const ROWS: u16 = 6;

/// One generated, emulator-supported ANSI operation.
///
/// The variants map 1:1 to sequences the emulator interprets per Requirement
/// 10.3: absolute cursor positioning (CUP), SGR color/attribute selection, and
/// printing a glyph. Keeping the alphabet to this set is what lets the test's
/// reference model stay simple and exact.
#[derive(Debug, Clone)]
enum AnsiOp {
    /// `CSI row;col H` — 1-based absolute positioning (stored here 0-based,
    /// already clamped to the viewport).
    MoveTo { row: u16, col: u16 },
    /// `CSI 0 m` — reset style.
    Reset,
    /// `CSI 1 m` — bold on.
    Bold,
    /// `CSI 4 m` — underline on.
    Underline,
    /// `CSI 3n m` — standard ANSI foreground (offset `0..=7`).
    Fg(u16),
    /// `CSI 4n m` — standard ANSI background (offset `0..=7`).
    Bg(u16),
    /// Print a printable ASCII glyph.
    Print(char),
}

impl AnsiOp {
    /// Encode this op as the raw bytes the emulator would receive.
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            // Convert 0-based back to the 1-based wire form.
            AnsiOp::MoveTo { row, col } => {
                out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
            }
            AnsiOp::Reset => out.extend_from_slice(b"\x1b[0m"),
            AnsiOp::Bold => out.extend_from_slice(b"\x1b[1m"),
            AnsiOp::Underline => out.extend_from_slice(b"\x1b[4m"),
            AnsiOp::Fg(n) => out.extend_from_slice(format!("\x1b[{}m", 30 + n).as_bytes()),
            AnsiOp::Bg(n) => out.extend_from_slice(format!("\x1b[{}m", 40 + n).as_bytes()),
            AnsiOp::Print(c) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
}

/// An independent reference model of the screen for Property 26.
///
/// It reproduces the emulator's documented behavior for the supported op set
/// using plain data structures, so a disagreement points at the emulator, not
/// at shared code.
struct RefModel {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
    cursor: Cursor,
    style: CellStyle,
}

impl RefModel {
    fn new(cols: u16, rows: u16) -> Self {
        RefModel {
            cols,
            rows,
            cells: vec![Cell::blank(); cols as usize * rows as usize],
            cursor: Cursor { row: 0, col: 0 },
            style: CellStyle::default(),
        }
    }

    fn idx(&self, row: u16, col: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    /// Shift every row up by one, blanking the new bottom row. Mirrors
    /// `Grid::scroll_up` (the cursor row is left unchanged by the caller).
    fn scroll_up(&mut self) {
        let w = self.cols as usize;
        self.cells.copy_within(w.., 0);
        let start = (self.rows as usize - 1) * w;
        for cell in &mut self.cells[start..] {
            *cell = Cell::blank();
        }
    }

    fn apply(&mut self, op: &AnsiOp) {
        match op {
            AnsiOp::MoveTo { row, col } => {
                // Mirror Grid::move_to's clamping.
                self.cursor.row = (*row).min(self.rows - 1);
                self.cursor.col = (*col).min(self.cols - 1);
            }
            AnsiOp::Reset => self.style = CellStyle::default(),
            AnsiOp::Bold => self.style.bold = true,
            AnsiOp::Underline => self.style.underline = true,
            AnsiOp::Fg(n) => {
                self.style.foreground = Color::Ansi(AnsiColor::from_standard_offset(*n).unwrap());
            }
            AnsiOp::Bg(n) => {
                self.style.background = Color::Ansi(AnsiColor::from_standard_offset(*n).unwrap());
            }
            AnsiOp::Print(c) => {
                let i = self.idx(self.cursor.row, self.cursor.col);
                self.cells[i] = Cell {
                    glyph: *c,
                    style: self.style,
                };
                // Mirror Grid::write's advance + wrap + scroll exactly.
                if self.cursor.col + 1 >= self.cols {
                    self.cursor.col = 0;
                    if self.cursor.row + 1 >= self.rows {
                        // At the bottom row the screen scrolls up by one: every
                        // row shifts up, a blank row appears at the bottom, and
                        // the cursor stays on the (new) bottom row.
                        self.scroll_up();
                    } else {
                        self.cursor.row += 1;
                    }
                } else {
                    self.cursor.col += 1;
                }
            }
        }
    }
}

/// A single supported ANSI op, confined to the viewport and color range.
fn ansi_op_strategy() -> impl Strategy<Value = AnsiOp> {
    prop_oneof![
        (0..ROWS, 0..COLS).prop_map(|(row, col)| AnsiOp::MoveTo { row, col }),
        Just(AnsiOp::Reset),
        Just(AnsiOp::Bold),
        Just(AnsiOp::Underline),
        (0u16..8).prop_map(AnsiOp::Fg),
        (0u16..8).prop_map(AnsiOp::Bg),
        // Printable ASCII glyphs only (exclude space/control to keep writes
        // visible and comparable).
        (b'!'..=b'~').prop_map(|b| AnsiOp::Print(b as char)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // Feature: remote-connection-manager, Property 26: ANSI interpretation matches the model
    //
    // For any stream of valid ANSI escape sequences for cursor positioning,
    // foreground/background color, and bold/underline styling, the resulting
    // terminal grid state (cursor position, cell colors, cell styles) matches a
    // reference model of those sequences.
    //
    // Validates: Requirements 10.3
    #[test]
    fn prop26_ansi_interpretation_matches_the_model(ops in prop::collection::vec(ansi_op_strategy(), 0..80)) {
        let mut term = TerminalEmulator::new(COLS, ROWS);
        let mut model = RefModel::new(COLS, ROWS);

        let mut bytes = Vec::new();
        for op in &ops {
            op.encode(&mut bytes);
            model.apply(op);
        }
        term.advance(&bytes);

        // Cursor position must match.
        prop_assert_eq!(term.cursor(), model.cursor, "cursor mismatch after ops: {:?}", ops);

        // Every cell's glyph, color, and style must match.
        for row in 0..ROWS {
            for col in 0..COLS {
                let got = term.grid().cell(row, col).unwrap();
                let want = &model.cells[model.idx(row, col)];
                prop_assert_eq!(got, want, "cell ({},{}) mismatch after ops: {:?}", row, col, ops);
            }
        }
    }
}

// --- Property 27: malformed sequences are ignored --------------------------

/// A fragment of valid, grid-affecting output: printable text, newlines, cursor
/// moves, and SGR. These compose into a "valid output stream".
fn valid_fragment_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // A short run of printable ASCII text.
        prop::collection::vec(b'a'..=b'z', 1..6),
        Just(b"\r\n".to_vec()),
        Just(b"\x1b[2;3H".to_vec()),
        Just(b"\x1b[1;31mX".to_vec()),
        Just(b"\x1b[0m".to_vec()),
        Just(b"\x1b[4;32mY".to_vec()),
    ]
}

/// A malformed / unrecognized escape sequence the emulator must discard.
///
/// These are either CSI sequences with an unsupported final byte or absurd
/// parameters, or escape sequences the emulator does not implement at all. None
/// of them should alter grid state.
fn malformed_sequence_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // CSI with an unknown final byte.
        Just(b"\x1b[Z".to_vec()),
        Just(b"\x1b[?25l".to_vec()),
        Just(b"\x1b[6n".to_vec()),
        // Absurdly large / overflowing parameters on an unknown final.
        Just(b"\x1b[9999999999;0q".to_vec()),
        // A private-mode CSI with an unknown final byte (self-delimiting).
        Just(b"\x1b[?1049h".to_vec()),
        // Escape sequences the emulator leaves as no-ops (OSC, charset).
        // All candidates here are self-delimiting: unlike a bare ESC (which
        // would absorb the following byte as part of an incomplete escape),
        // each resolves fully on its own, so splicing it between valid
        // fragments must not disturb the surrounding output.
        Just(b"\x1b]0;title\x07".to_vec()),
        Just(b"\x1b(B".to_vec()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // Feature: remote-connection-manager, Property 27: Malformed escape sequences are ignored without corruption
    //
    // For any valid output stream, injecting a malformed or unrecognized escape
    // sequence produces the same terminal grid state as the stream without the
    // injected sequence, and parsing of subsequent output continues.
    //
    // Validates: Requirements 10.4
    #[test]
    fn prop27_malformed_sequences_are_ignored(
        fragments in prop::collection::vec(valid_fragment_strategy(), 1..12),
        malformed in malformed_sequence_strategy(),
        inject_at in any::<prop::sample::Index>(),
    ) {
        // Baseline: the valid stream with nothing injected.
        let mut baseline = TerminalEmulator::new(10, 4);
        for f in &fragments {
            baseline.advance(f);
        }

        // Injected: the same stream, with the malformed sequence spliced in at
        // some boundary between fragments. Splicing at a boundary keeps both
        // streams byte-identical except for the injected bytes.
        let pos = inject_at.index(fragments.len() + 1);
        let mut injected = TerminalEmulator::new(10, 4);
        for (i, f) in fragments.iter().enumerate() {
            if i == pos {
                injected.advance(&malformed);
            }
            injected.advance(f);
        }
        if pos >= fragments.len() {
            injected.advance(&malformed);
        }

        // Grid contents, cursor, and scrollback must be unchanged: the
        // malformed sequence was a no-op and parsing continued.
        prop_assert_eq!(
            injected.grid().rows_as_strings(),
            baseline.grid().rows_as_strings(),
            "grid diverged after injecting {:?}", malformed
        );
        prop_assert_eq!(injected.cursor(), baseline.cursor());
        prop_assert_eq!(injected.scrollback().len(), baseline.scrollback().len());

        // Parsing continues: appending more valid output still lands.
        baseline.advance(b"\x1b[1;1HZ");
        injected.advance(b"\x1b[1;1HZ");
        prop_assert_eq!(
            injected.grid().rows_as_strings(),
            baseline.grid().rows_as_strings()
        );
    }
}

// --- Property 28: resize reports the new dimensions ------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // Feature: remote-connection-manager, Property 28: Resize reports the new dimensions
    //
    // For any viewport resize, the terminal dimensions reported to the remote
    // host equal the new viewport's columns and rows.
    //
    // Validates: Requirements 10.5
    #[test]
    fn prop28_resize_reports_new_dimensions(
        start_cols in 1u16..200,
        start_rows in 1u16..200,
        new_cols in 0u16..400,
        new_rows in 0u16..400,
    ) {
        let mut term = TerminalEmulator::new(start_cols, start_rows);

        // The grid floors dimensions at 1, so the reported dims use the same
        // clamp. This is the value the remote is told about.
        let expected = Dimensions {
            cols: new_cols.max(1),
            rows: new_rows.max(1),
        };

        let returned = term.resize(new_cols, new_rows);
        prop_assert_eq!(returned, expected, "resize() return value");
        prop_assert_eq!(term.dimensions(), expected, "dimensions() after resize");

        // The report forwarded to the remote equals the new dimensions, and is
        // drained exactly once.
        prop_assert_eq!(term.take_resize_report(), Some(expected), "resize report");
        prop_assert_eq!(term.take_resize_report(), None, "report drained once");
    }
}

// --- Property 29: scrollback is a bounded FIFO -----------------------------

/// Build a one-cell line tagged with a sequence number so retained lines are
/// individually identifiable. The number is encoded in the glyph via a wide
/// char so distinct ids stay distinct.
fn tagged_line(id: u32) -> Vec<Cell> {
    // Map id into a distinct, valid char. Using the id directly as a scalar
    // value, skipping the surrogate range, keeps every id distinguishable.
    let scalar = if id >= 0xD800 { id + 0x800 } else { id };
    let glyph = char::from_u32(scalar).unwrap_or('?');
    vec![Cell {
        glyph,
        ..Cell::blank()
    }]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    // Feature: remote-connection-manager, Property 29: Scrollback is a bounded FIFO buffer
    //
    // For any sequence of appended output lines, the scrollback length never
    // exceeds its cap (at least 1000 lines) and the retained lines are exactly
    // the most recent lines in order, with the oldest discarded first.
    //
    // Validates: Requirements 10.6
    #[test]
    fn prop29_scrollback_is_bounded_fifo(
        // Capacity requested by the caller; the buffer floors it at the minimum.
        requested_cap in 0usize..2500,
        // How many lines we push (sometimes well past capacity).
        push_count in 0u32..3200,
    ) {
        let mut buf = ScrollbackBuffer::with_capacity(requested_cap);
        let cap = buf.capacity();

        // Capacity honors the >= 1000 floor from Requirement 10.6.
        prop_assert!(cap >= DEFAULT_SCROLLBACK_LINES, "capacity floored at minimum");
        prop_assert_eq!(cap, requested_cap.max(DEFAULT_SCROLLBACK_LINES));

        // Independent model of the expected retained ids: the most recent
        // `min(push_count, cap)` ids, in push order.
        let mut expected_ids: std::collections::VecDeque<u32> = std::collections::VecDeque::new();

        for id in 0..push_count {
            buf.push(tagged_line(id));
            expected_ids.push_back(id);
            if expected_ids.len() > cap {
                expected_ids.pop_front();
            }
            // Invariant holds after every push: never exceeds capacity.
            prop_assert!(buf.len() <= cap, "len {} exceeded cap {}", buf.len(), cap);
        }

        // Final length is min(push_count, cap).
        let expected_len = (push_count as usize).min(cap);
        prop_assert_eq!(buf.len(), expected_len, "final length");

        // Retained lines are exactly the most recent ids, in order, oldest
        // (lowest surviving id) first.
        let got_ids: Vec<char> = buf.iter().map(|l| l[0].glyph).collect();
        let want_ids: Vec<char> = expected_ids.iter().map(|id| tagged_line(*id)[0].glyph).collect();
        prop_assert_eq!(got_ids, want_ids, "retained lines mismatch");
    }
}
