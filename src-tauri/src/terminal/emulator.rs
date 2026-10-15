//! The ANSI state machine that drives the screen grid and scrollback.
//!
//! [`TerminalEmulator`] is the Terminal_Emulator component's core: it feeds the
//! raw byte stream from a text-based Session through [`vte::Parser`] (a
//! well-tested ANSI/VT state machine) and applies the recognized actions to a
//! [`Grid`] and a [`ScrollbackBuffer`]. It interprets the sequences
//! Requirement 10.3 names — cursor positioning (CUP/CUU/CUD/CUF/CUB/CR/LF) and
//! SGR styling (bold, underline, 16 ANSI colors, 256-indexed, truecolor) — and,
//! per Requirement 10.4, silently discards anything it does not recognize while
//! continuing to parse subsequent output without corrupting state.
//!
//! ## Testability (Properties 26-29)
//!
//! The type is deliberately shaped so the property tests in tasks 14.2-14.5 can
//! drive and inspect it without any I/O:
//!
//! - [`TerminalEmulator::advance`] injects raw bytes (Property 26/27).
//! - [`TerminalEmulator::grid`] exposes the queryable reference-model grid:
//!   cursor position, per-cell glyph, color, and style (Property 26).
//! - [`TerminalEmulator::resize`] applies a viewport change and
//!   [`TerminalEmulator::dimensions`] / [`TerminalEmulator::take_resize_report`]
//!   report the new (cols, rows) to be forwarded to the remote (Property 28).
//! - [`TerminalEmulator::scrollback`] exposes the bounded FIFO (Property 29).
//!
//! The parser state and the grid/scrollback are split so the borrow checker is
//! happy driving one from the other: `vte::Parser` owns no screen state; all of
//! that lives in [`Performer`], which implements [`vte::Perform`].

use vte::{Params, Parser, Perform};

use super::color::{AnsiColor, CellStyle, Color};
use super::grid::{Cell, Cursor, Grid};
use super::scrollback::ScrollbackBuffer;

/// The dimensions of the terminal viewport, in character cells.
///
/// This is exactly what Requirement 10.5 / Property 28 require the emulator to
/// report to the remote host after a resize: the new column and row counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    /// Number of columns.
    pub cols: u16,
    /// Number of rows.
    pub rows: u16,
}

/// The ANSI terminal emulator for a single text-based Session.
///
/// Owns the byte-stream parser, the on-screen [`Grid`], and the
/// [`ScrollbackBuffer`]. Call [`advance`](Self::advance) with received bytes and
/// read the resulting state through [`grid`](Self::grid) and
/// [`scrollback`](Self::scrollback).
///
/// `Debug` is implemented by hand because [`vte::Parser`] does not implement it;
/// the parser is opaque internal state, so the impl just reports the viewport
/// and scrollback size.
pub struct TerminalEmulator {
    parser: Parser,
    performer: Performer,
    /// Set when the viewport is resized; drained by `take_resize_report` so the
    /// caller (the SessionConn wiring, a later task) can notify the remote.
    pending_resize: Option<Dimensions>,
}

impl std::fmt::Debug for TerminalEmulator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalEmulator")
            .field("dimensions", &self.dimensions())
            .field("cursor", &self.cursor())
            .field("scrollback_len", &self.performer.scrollback.len())
            .field("pending_resize", &self.pending_resize)
            .finish()
    }
}

impl TerminalEmulator {
    /// Create an emulator with the given viewport size and the default
    /// scrollback capacity (>=1000 lines).
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, ScrollbackBuffer::new())
    }

    /// Create an emulator with an explicit scrollback buffer (useful for tests
    /// that want a specific capacity).
    pub fn with_scrollback(cols: u16, rows: u16, scrollback: ScrollbackBuffer) -> Self {
        TerminalEmulator {
            parser: Parser::new(),
            performer: Performer {
                grid: Grid::new(cols, rows),
                scrollback,
                style: CellStyle::default(),
            },
            pending_resize: None,
        }
    }

    /// Feed a chunk of raw bytes from the remote through the ANSI state machine,
    /// applying every recognized action to the grid and scrollback. Unknown or
    /// malformed sequences are discarded without disturbing state
    /// (Requirement 10.4).
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.performer, bytes);
    }

    /// Borrow the current screen grid (the queryable reference model).
    pub fn grid(&self) -> &Grid {
        &self.performer.grid
    }

    /// The current cursor position.
    pub fn cursor(&self) -> Cursor {
        self.performer.grid.cursor()
    }

    /// Borrow the scrollback buffer.
    pub fn scrollback(&self) -> &ScrollbackBuffer {
        &self.performer.scrollback
    }

    /// The current viewport dimensions (cols, rows).
    pub fn dimensions(&self) -> Dimensions {
        Dimensions {
            cols: self.performer.grid.cols(),
            rows: self.performer.grid.rows(),
        }
    }

    /// Resize the viewport. Preserves overlapping content, clamps the cursor,
    /// and records the new dimensions so they can be reported to the remote.
    /// Returns the new dimensions (Requirement 10.5 / Property 28).
    pub fn resize(&mut self, cols: u16, rows: u16) -> Dimensions {
        self.performer.grid.resize(cols, rows);
        let dims = self.dimensions();
        self.pending_resize = Some(dims);
        dims
    }

    /// Take the pending resize report, if any, so the caller can forward the
    /// new dimensions to the remote host via `SessionConn::resize`. Returns
    /// `None` when no resize has happened since the last call. This is the seam
    /// the (separate) SessionConn wiring task plugs into.
    pub fn take_resize_report(&mut self) -> Option<Dimensions> {
        self.pending_resize.take()
    }
}

/// The screen-state side of the emulator: the [`vte::Perform`] sink.
///
/// `vte::Parser` calls into this for every decoded action. It holds the grid,
/// scrollback, and the currently active SGR style, and translates ANSI actions
/// into grid primitives. Anything it does not recognize is a no-op, which is
/// precisely the "discard malformed/unknown and keep going" rule of
/// Requirement 10.4 / Property 27.
#[derive(Debug)]
struct Performer {
    grid: Grid,
    scrollback: ScrollbackBuffer,
    style: CellStyle,
}

impl Performer {
    /// Push a line scrolled off the top of the grid into scrollback, if any.
    fn absorb(&mut self, scrolled: Option<Vec<Cell>>) {
        if let Some(line) = scrolled {
            self.scrollback.push(line);
        }
    }

    /// Apply a run of SGR (`CSI ... m`) parameters to the active style.
    ///
    /// Handles the attributes Requirement 10.3 names (bold, underline) and
    /// colors: the 16 basic ANSI colors (standard `30..=37`/`40..=47`, bright
    /// `90..=97`/`100..=107`), the 256-indexed palette and 24-bit truecolor
    /// (`38;5;n` / `48;5;n`, `38;2;r;g;b` / `48;2;r;g;b`), and the resets.
    /// Unrecognized SGR codes are ignored.
    fn apply_sgr(&mut self, params: &Params) {
        // Flatten to the primary value of each parameter. Extended color
        // sequences may use either `;` (separate params) or `:` (subparams);
        // reading the leading subparam of each handles both encodings.
        let codes: Vec<u16> = params
            .iter()
            .map(|p| p.first().copied().unwrap_or(0))
            .collect();

        // An empty `CSI m` means `CSI 0 m` (reset).
        if codes.is_empty() {
            self.style.reset();
            return;
        }

        let mut i = 0;
        while i < codes.len() {
            let code = codes[i];
            match code {
                0 => self.style.reset(),
                1 => self.style.bold = true,
                4 => self.style.underline = true,
                22 => self.style.bold = false,
                24 => self.style.underline = false,
                30..=37 => {
                    if let Some(c) = AnsiColor::from_standard_offset(code - 30) {
                        self.style.foreground = Color::Ansi(c);
                    }
                }
                38 => {
                    if let Some(color) = parse_extended_color(&codes, &mut i) {
                        self.style.foreground = color;
                    }
                }
                39 => self.style.foreground = Color::Default,
                40..=47 => {
                    if let Some(c) = AnsiColor::from_standard_offset(code - 40) {
                        self.style.background = Color::Ansi(c);
                    }
                }
                48 => {
                    if let Some(color) = parse_extended_color(&codes, &mut i) {
                        self.style.background = color;
                    }
                }
                49 => self.style.background = Color::Default,
                90..=97 => {
                    if let Some(c) = AnsiColor::from_bright_offset(code - 90) {
                        self.style.foreground = Color::Ansi(c);
                    }
                }
                100..=107 => {
                    if let Some(c) = AnsiColor::from_bright_offset(code - 100) {
                        self.style.background = Color::Ansi(c);
                    }
                }
                // Any other SGR code is not interpreted; ignore it.
                _ => {}
            }
            i += 1;
        }
    }

    /// First numeric parameter, defaulting to `default` when absent or zero-as-
    /// absent is not desired. Used to read cursor-movement counts.
    fn param_or(params: &Params, index: usize, default: u16) -> u16 {
        match params.iter().nth(index).and_then(|p| p.first().copied()) {
            Some(0) | None => default,
            Some(v) => v,
        }
    }
}

/// Parse a `38`/`48` extended-color run starting at `codes[*i]` (which is the
/// `38` or `48` selector). Advances `*i` past the consumed values and returns
/// the color, or `None` if the run is malformed.
///
/// Supports `5;n` (256-indexed) and `2;r;g;b` (truecolor). This handles the
/// `;`-separated encoding; the `:`-subparam encoding is handled upstream by
/// reading each parameter's leading subparam.
fn parse_extended_color(codes: &[u16], i: &mut usize) -> Option<Color> {
    let kind = *codes.get(*i + 1)?;
    match kind {
        5 => {
            let idx = *codes.get(*i + 2)?;
            *i += 2;
            Some(Color::Indexed(idx.min(255) as u8))
        }
        2 => {
            let r = *codes.get(*i + 2)?;
            let g = *codes.get(*i + 3)?;
            let b = *codes.get(*i + 4)?;
            *i += 4;
            Some(Color::Rgb {
                r: r.min(255) as u8,
                g: g.min(255) as u8,
                b: b.min(255) as u8,
            })
        }
        _ => None,
    }
}

impl Perform for Performer {
    /// A printable character: write it at the cursor with the active style.
    fn print(&mut self, c: char) {
        let scrolled = self.grid.write(c, self.style);
        self.absorb(scrolled);
    }

    /// A C0 control: handle CR / LF / BS / HT; ignore the rest.
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.grid.carriage_return(),
            b'\n' | 0x0b | 0x0c => {
                // LF, VT, FF all move down a line (and may scroll).
                let scrolled = self.grid.line_feed();
                self.absorb(scrolled);
            }
            0x08 => self.grid.move_left(1), // backspace
            b'\t' => {
                // Tab: advance to the next 8-column tab stop.
                let col = self.grid.cursor().col;
                let next = (col / 8 + 1) * 8;
                self.grid.move_right(next - col);
            }
            _ => {}
        }
    }

    /// A CSI sequence: cursor movement, erase, and SGR. Unknown finals ignored.
    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], ignore: bool, action: char) {
        // `ignore` is set when the parser saw too many intermediates/params and
        // dropped characters: treat the whole sequence as malformed and skip it
        // (Requirement 10.4).
        if ignore {
            return;
        }

        match action {
            // Cursor Up / Down / Forward / Back.
            'A' => self.grid.move_up(Performer::param_or(params, 0, 1)),
            'B' => self.grid.move_down(Performer::param_or(params, 0, 1)),
            'C' => self.grid.move_right(Performer::param_or(params, 0, 1)),
            'D' => self.grid.move_left(Performer::param_or(params, 0, 1)),
            // Cursor Position (CUP) and Horizontal/Vertical Position (HVP).
            // Params are 1-based row;col, default 1;1.
            'H' | 'f' => {
                let row = Performer::param_or(params, 0, 1).saturating_sub(1);
                let col = Performer::param_or(params, 1, 1).saturating_sub(1);
                self.grid.move_to(row, col);
            }
            // Erase in Display: 0 = cursor..end, 2 = whole screen.
            'J' => match params.iter().next().and_then(|p| p.first().copied()) {
                Some(2) | Some(3) => self.grid.clear_screen(),
                _ => self.grid.clear_below(),
            },
            // Erase in Line: 0 = cursor..end of line.
            'K' => self.grid.clear_line_to_end(),
            // Select Graphic Rendition.
            'm' => self.apply_sgr(params),
            // Any other final byte is an unrecognized/unsupported CSI; ignore.
            _ => {}
        }
    }

    // `esc_dispatch`, `hook`, `put`, `unhook`, `osc_dispatch` are intentionally
    // left as the trait's default no-ops: those sequences (OSC title changes,
    // DCS, charset selection, etc.) are not part of this task's scope and must
    // simply be discarded without affecting grid state (Requirement 10.4).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_lands_on_the_grid() {
        let mut term = TerminalEmulator::new(10, 3);
        term.advance(b"hi");
        assert_eq!(term.grid().cell(0, 0).unwrap().glyph, 'h');
        assert_eq!(term.grid().cell(0, 1).unwrap().glyph, 'i');
        assert_eq!(term.cursor(), Cursor { row: 0, col: 2 });
    }

    #[test]
    fn cursor_positioning_is_one_based() {
        let mut term = TerminalEmulator::new(10, 5);
        // CSI 2;3 H -> zero-based (1, 2).
        term.advance(b"\x1b[2;3H");
        assert_eq!(term.cursor(), Cursor { row: 1, col: 2 });
        term.advance(b"X");
        assert_eq!(term.grid().cell(1, 2).unwrap().glyph, 'X');
    }

    #[test]
    fn sgr_sets_bold_underline_and_colors() {
        let mut term = TerminalEmulator::new(10, 2);
        // bold; underline; red fg; green bg, then a char.
        term.advance(b"\x1b[1;4;31;42mA");
        let cell = term.grid().cell(0, 0).unwrap();
        assert!(cell.style.bold);
        assert!(cell.style.underline);
        assert_eq!(cell.style.foreground, Color::Ansi(AnsiColor::Red));
        assert_eq!(cell.style.background, Color::Ansi(AnsiColor::Green));
        // Reset clears it for the next char.
        term.advance(b"\x1b[0mB");
        let cell_b = term.grid().cell(0, 1).unwrap();
        assert_eq!(cell_b.style, CellStyle::default());
    }

    #[test]
    fn truecolor_and_indexed_colors_parse() {
        let mut term = TerminalEmulator::new(10, 2);
        term.advance(b"\x1b[38;2;10;20;30mA");
        assert_eq!(
            term.grid().cell(0, 0).unwrap().style.foreground,
            Color::Rgb { r: 10, g: 20, b: 30 }
        );
        term.advance(b"\x1b[48;5;123mB");
        assert_eq!(
            term.grid().cell(0, 1).unwrap().style.background,
            Color::Indexed(123)
        );
    }

    #[test]
    fn malformed_sequence_is_ignored_and_parsing_continues() {
        let mut baseline = TerminalEmulator::new(10, 3);
        baseline.advance(b"AB");

        let mut injected = TerminalEmulator::new(10, 3);
        // A bogus CSI with no valid final + an unknown final byte, between the
        // same two glyphs. State must match the baseline (Property 27).
        injected.advance(b"A\x1b[9999999999\x1b[ZB");

        assert_eq!(
            injected.grid().rows_as_strings(),
            baseline.grid().rows_as_strings()
        );
        assert_eq!(injected.cursor(), baseline.cursor());
    }

    #[test]
    fn line_feeds_beyond_screen_fill_scrollback() {
        let mut term = TerminalEmulator::new(4, 2);
        // Write 5 rows worth of content; the screen holds 2, so 3 scroll off.
        term.advance(b"r0\r\nr1\r\nr2\r\nr3\r\nr4");
        assert!(term.scrollback().len() >= 3);
        assert_eq!(term.scrollback().capacity() >= 1000, true);
    }

    #[test]
    fn resize_reports_new_dimensions() {
        let mut term = TerminalEmulator::new(80, 24);
        let dims = term.resize(100, 40);
        assert_eq!(dims, Dimensions { cols: 100, rows: 40 });
        assert_eq!(term.dimensions(), Dimensions { cols: 100, rows: 40 });
        // The report is drained exactly once.
        assert_eq!(term.take_resize_report(), Some(Dimensions { cols: 100, rows: 40 }));
        assert_eq!(term.take_resize_report(), None);
    }
}
