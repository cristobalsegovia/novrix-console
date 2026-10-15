//! The terminal screen grid and cursor.
//!
//! The [`Grid`] is the "reference model" of the visible screen that Property 26
//! compares against: a `rows x cols` matrix of [`Cell`]s plus a [`Cursor`]. It
//! owns no parsing logic — it only exposes the primitive editing operations the
//! emulator's ANSI state machine drives (move the cursor, write a glyph, erase,
//! scroll). Keeping these as small, deterministic methods is what makes the
//! grid directly queryable from the property tests (tasks 14.2-14.5).
//!
//! Coordinates are **zero-based** internally: `row` in `0..rows`, `col` in
//! `0..cols`. ANSI sequences are one-based; the emulator converts at the
//! boundary.

use serde::{Deserialize, Serialize};

use super::color::CellStyle;

/// A single screen cell: the glyph it displays and the style applied to it.
///
/// A blank cell holds a space (`' '`) with the default [`CellStyle`]. The glyph
/// is a full `char` so wide/Unicode content round-trips through the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    /// The character displayed in this cell.
    pub glyph: char,
    /// The style (colors + attributes) applied to this cell.
    pub style: CellStyle,
}

impl Cell {
    /// A blank cell: a space with the default style.
    pub fn blank() -> Self {
        Cell {
            glyph: ' ',
            style: CellStyle::default(),
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::blank()
    }
}

/// The cursor position within the grid, in zero-based `(row, col)` coordinates.
///
/// The cursor is always clamped into the grid's bounds, so `row < rows` and
/// `col < cols` hold for any grid it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cursor {
    /// Zero-based row.
    pub row: u16,
    /// Zero-based column.
    pub col: u16,
}

/// The visible terminal screen: a fixed-size cell matrix plus the cursor.
///
/// All editing is done through methods that keep the invariants intact (cursor
/// in bounds, every row exactly `cols` wide). The emulator never reaches into
/// `cells` directly for mutation; it calls these primitives, which is what lets
/// a reference model reproduce the same state from the same sequence of calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grid {
    cols: u16,
    rows: u16,
    /// Row-major cells, `rows * cols` entries. `cells[r * cols + c]`.
    cells: Vec<Cell>,
    cursor: Cursor,
}

impl Grid {
    /// Create a blank grid of the given dimensions.
    ///
    /// `cols` and `rows` are clamped to a minimum of 1 so the grid is never
    /// degenerate (zero-area), which keeps all the index math well-defined.
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Grid {
            cols,
            rows,
            cells: vec![Cell::blank(); cols as usize * rows as usize],
            cursor: Cursor::default(),
        }
    }

    /// Number of columns.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Number of rows.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// The current cursor position.
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Borrow the cell at zero-based `(row, col)`, or `None` if out of bounds.
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells.get(self.index(row, col))
    }

    /// All rows as text, top to bottom, with trailing blanks on each row
    /// preserved up to the full width. Useful for assertions and snapshots.
    pub fn rows_as_strings(&self) -> Vec<String> {
        (0..self.rows)
            .map(|r| {
                (0..self.cols)
                    .map(|c| self.cells[self.index(r, c)].glyph)
                    .collect()
            })
            .collect()
    }

    fn index(&self, row: u16, col: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    // --- Cursor movement ---------------------------------------------------

    /// Move the cursor to an absolute zero-based position, clamped to bounds.
    pub fn move_to(&mut self, row: u16, col: u16) {
        self.cursor.row = row.min(self.rows - 1);
        self.cursor.col = col.min(self.cols - 1);
    }

    /// Move the cursor up by `n` rows (clamped at the top edge).
    pub fn move_up(&mut self, n: u16) {
        self.cursor.row = self.cursor.row.saturating_sub(n);
    }

    /// Move the cursor down by `n` rows (clamped at the bottom edge).
    pub fn move_down(&mut self, n: u16) {
        self.cursor.row = (self.cursor.row + n).min(self.rows - 1);
    }

    /// Move the cursor right by `n` columns (clamped at the right edge).
    pub fn move_right(&mut self, n: u16) {
        self.cursor.col = (self.cursor.col + n).min(self.cols - 1);
    }

    /// Move the cursor left by `n` columns (clamped at the left edge).
    pub fn move_left(&mut self, n: u16) {
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    /// Move to the start of the current line (carriage return).
    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
    }

    // --- Writing -----------------------------------------------------------

    /// Write a glyph at the cursor with `style`, then advance the cursor one
    /// column. At the right edge the cursor wraps to the start of the next
    /// line, scrolling the content up (returning the line scrolled off, if any)
    /// when it would move past the bottom row.
    ///
    /// The scrolled-off line is returned so the emulator can push it into the
    /// scrollback buffer; `None` means nothing scrolled.
    #[must_use = "the returned line must be pushed to scrollback"]
    pub fn write(&mut self, glyph: char, style: CellStyle) -> Option<Vec<Cell>> {
        let idx = self.index(self.cursor.row, self.cursor.col);
        self.cells[idx] = Cell { glyph, style };

        if self.cursor.col + 1 >= self.cols {
            // Wrap to next line.
            self.cursor.col = 0;
            self.advance_row()
        } else {
            self.cursor.col += 1;
            None
        }
    }

    /// Move the cursor to the next row, scrolling up (and returning the line
    /// that fell off the top) if it is already on the bottom row.
    #[must_use = "the returned line must be pushed to scrollback"]
    pub fn advance_row(&mut self) -> Option<Vec<Cell>> {
        if self.cursor.row + 1 >= self.rows {
            self.scroll_up()
        } else {
            self.cursor.row += 1;
            None
        }
    }

    /// A line feed: behaves like [`advance_row`](Self::advance_row), scrolling
    /// when at the bottom. Returns the scrolled-off line, if any.
    #[must_use = "the returned line must be pushed to scrollback"]
    pub fn line_feed(&mut self) -> Option<Vec<Cell>> {
        self.advance_row()
    }

    /// Scroll the whole screen up by one row: the top row is removed and
    /// returned, every other row shifts up, and a fresh blank row appears at
    /// the bottom. The cursor row is unchanged (it stays on what is now the
    /// bottom row).
    #[must_use = "the returned line is the content scrolled off the top"]
    pub fn scroll_up(&mut self) -> Option<Vec<Cell>> {
        let top: Vec<Cell> = self.cells[0..self.cols as usize].to_vec();
        // Shift rows 1.. up into 0..
        self.cells.copy_within(self.cols as usize.., 0);
        // Blank the new bottom row.
        let start = (self.rows as usize - 1) * self.cols as usize;
        for cell in &mut self.cells[start..] {
            *cell = Cell::blank();
        }
        Some(top)
    }

    // --- Erasing -----------------------------------------------------------

    /// Clear the entire screen to blanks. The cursor position is unchanged.
    pub fn clear_screen(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::blank();
        }
    }

    /// Clear from the cursor to the end of the screen (inclusive of the cursor
    /// cell), the behavior of `CSI 0 J`.
    pub fn clear_below(&mut self) {
        let start = self.index(self.cursor.row, self.cursor.col);
        for cell in &mut self.cells[start..] {
            *cell = Cell::blank();
        }
    }

    /// Clear the current line from the cursor to the end of the row, the
    /// behavior of `CSI 0 K`.
    pub fn clear_line_to_end(&mut self) {
        let start = self.index(self.cursor.row, self.cursor.col);
        let end = self.index(self.cursor.row, self.cols - 1) + 1;
        for cell in &mut self.cells[start..end] {
            *cell = Cell::blank();
        }
    }

    // --- Resize ------------------------------------------------------------

    /// Resize the grid to new dimensions, preserving overlapping content from
    /// the top-left. New area is filled with blanks; the cursor is clamped into
    /// the new bounds.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }

        let mut next = vec![Cell::blank(); cols as usize * rows as usize];
        let copy_rows = rows.min(self.rows);
        let copy_cols = cols.min(self.cols);
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                next[r as usize * cols as usize + c as usize] = self.cells[self.index(r, c)];
            }
        }

        self.cols = cols;
        self.rows = rows;
        self.cells = next;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_grid_is_blank_with_cursor_at_origin() {
        let g = Grid::new(4, 3);
        assert_eq!(g.cols(), 4);
        assert_eq!(g.rows(), 3);
        assert_eq!(g.cursor(), Cursor { row: 0, col: 0 });
        assert_eq!(g.cell(2, 3), Some(&Cell::blank()));
        assert_eq!(g.cell(3, 0), None);
    }

    #[test]
    fn write_advances_and_wraps() {
        let mut g = Grid::new(2, 2);
        assert!(g.write('a', CellStyle::default()).is_none());
        assert_eq!(g.cursor(), Cursor { row: 0, col: 1 });
        // Writing at the right edge wraps to the next line.
        assert!(g.write('b', CellStyle::default()).is_none());
        assert_eq!(g.cursor(), Cursor { row: 1, col: 0 });
        assert_eq!(g.cell(0, 0).unwrap().glyph, 'a');
        assert_eq!(g.cell(0, 1).unwrap().glyph, 'b');
    }

    #[test]
    fn move_to_clamps_into_bounds() {
        let mut g = Grid::new(3, 3);
        g.move_to(99, 99);
        assert_eq!(g.cursor(), Cursor { row: 2, col: 2 });
    }

    #[test]
    fn scroll_up_returns_top_and_shifts() {
        let mut g = Grid::new(2, 2);
        // Fill row 0 with 'x'.
        let _ = g.write('x', CellStyle::default());
        let _ = g.write('x', CellStyle::default()); // wraps to row 1, col 0
        let scrolled = g.scroll_up().unwrap();
        assert_eq!(scrolled.len(), 2);
        assert!(scrolled.iter().all(|c| c.glyph == 'x'));
        // New bottom row is blank.
        assert_eq!(g.cell(1, 0).unwrap().glyph, ' ');
    }

    #[test]
    fn resize_preserves_top_left() {
        let mut g = Grid::new(3, 3);
        let _ = g.write('a', CellStyle::default());
        g.resize(2, 2);
        assert_eq!(g.cols(), 2);
        assert_eq!(g.rows(), 2);
        assert_eq!(g.cell(0, 0).unwrap().glyph, 'a');
    }
}
