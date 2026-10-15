//! The bounded FIFO scrollback buffer.
//!
//! As lines scroll off the top of the visible [`Grid`](super::grid::Grid), the
//! emulator pushes them here. The buffer is a first-in/first-out ring with a
//! fixed capacity: once full, pushing a new line discards the oldest one. This
//! is the exact behavior Property 29 checks — length never exceeds the cap, and
//! the retained lines are the most recent, in order, oldest discarded first.
//!
//! Requirement 10.6 mandates **at least** 1000 lines of history, so the
//! capacity is configurable but defaults to (and is floored at) 1000.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::grid::Cell;

/// The minimum (and default) scrollback capacity required by Requirement 10.6.
pub const DEFAULT_SCROLLBACK_LINES: usize = 1000;

/// A fixed-capacity FIFO buffer of scrolled-off lines.
///
/// A "line" is the full row of [`Cell`]s that scrolled off the top of the
/// screen, so color/style history is preserved alongside the glyphs. The buffer
/// never grows past `capacity`; the oldest line is dropped to make room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollbackBuffer {
    capacity: usize,
    lines: VecDeque<Vec<Cell>>,
}

impl ScrollbackBuffer {
    /// Create a buffer with the default capacity ([`DEFAULT_SCROLLBACK_LINES`]).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_SCROLLBACK_LINES)
    }

    /// Create a buffer with an explicit capacity.
    ///
    /// To honor Requirement 10.6 the capacity is floored at
    /// [`DEFAULT_SCROLLBACK_LINES`]; a smaller requested value is raised to the
    /// minimum rather than silently accepted.
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(DEFAULT_SCROLLBACK_LINES);
        ScrollbackBuffer {
            capacity,
            lines: VecDeque::with_capacity(capacity),
        }
    }

    /// The maximum number of lines this buffer retains.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The number of lines currently retained.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the buffer currently holds no lines.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Append a line that scrolled off the screen. If the buffer is at
    /// capacity, the oldest line is discarded first so the invariant
    /// `len() <= capacity()` always holds.
    pub fn push(&mut self, line: Vec<Cell>) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Iterate the retained lines from oldest to newest.
    pub fn iter(&self) -> impl Iterator<Item = &Vec<Cell>> {
        self.lines.iter()
    }

    /// Borrow the line at `index` counting from the oldest (`0`), or `None`.
    pub fn get(&self, index: usize) -> Option<&Vec<Cell>> {
        self.lines.get(index)
    }
}

impl Default for ScrollbackBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(glyph: char) -> Vec<Cell> {
        vec![Cell {
            glyph,
            ..Cell::blank()
        }]
    }

    #[test]
    fn capacity_is_floored_at_the_minimum() {
        let b = ScrollbackBuffer::with_capacity(10);
        assert_eq!(b.capacity(), DEFAULT_SCROLLBACK_LINES);
        let b = ScrollbackBuffer::with_capacity(5000);
        assert_eq!(b.capacity(), 5000);
    }

    #[test]
    fn push_discards_oldest_when_full() {
        let mut b = ScrollbackBuffer::with_capacity(DEFAULT_SCROLLBACK_LINES);
        let cap = b.capacity();
        // Fill to capacity + 1 with distinguishable lines.
        for i in 0..=cap {
            let ch = char::from_u32((i % 90 + 33) as u32).unwrap();
            b.push(line(ch));
        }
        // Never exceeds capacity.
        assert_eq!(b.len(), cap);
        // The very first line (i = 0) was discarded; the oldest retained line
        // is the one pushed at i = 1.
        let expected_oldest = char::from_u32((1 % 90 + 33) as u32).unwrap();
        assert_eq!(b.get(0).unwrap()[0].glyph, expected_oldest);
    }

    #[test]
    fn retains_most_recent_in_order() {
        let mut b = ScrollbackBuffer::with_capacity(DEFAULT_SCROLLBACK_LINES);
        b.push(line('a'));
        b.push(line('b'));
        b.push(line('c'));
        let glyphs: Vec<char> = b.iter().map(|l| l[0].glyph).collect();
        assert_eq!(glyphs, vec!['a', 'b', 'c']);
    }
}
