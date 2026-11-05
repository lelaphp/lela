//! Offset to line and column, both ways (design §4.1: the line table).
//! Source is raw bytes and a column is a byte count; php's own positions
//! are lines, and a byte column is what its scanner keeps.

/// A zero-based line and byte column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineCol {
    /// Zero-based line number.
    pub line: u32,
    /// Zero-based byte column.
    pub col: u32,
}

/// Maps byte offsets to line and column positions, both ways.
///
/// Construction is `O(len)`; a lookup is a binary search. Lines split on
/// `\n` only, so `\r\n` leaves the `\r` as the last column of its line and
/// every byte stays addressable. The table is in the arena of the parse
/// it serves ([`crate::Arena`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LineIndex<'a> {
    /// Byte offset of the first byte of each line after the first, which
    /// starts at 0: one entry per `\n`, so a text of one line, an `eval`
    /// of a statement, takes no room for its table. The default is the
    /// index of the empty text: one line, no bytes.
    breaks: &'a [u32],
    /// Total length in bytes.
    len: u32,
}

impl<'a> LineIndex<'a> {
    /// Builds the index for `text` in `arena`, which [`crate::load_bytes`]
    /// has held under 4 GiB; a longer text is indexed up to that length.
    /// The breaks are counted first, so the table is one allocation of
    /// its length.
    #[must_use]
    pub fn new(arena: &'a crate::Arena, text: &[u8]) -> LineIndex<'a> {
        let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
        let text = text.get(..len as usize).unwrap_or(text);
        #[allow(clippy::naive_bytecount)] // a count the compiler vectorises; no package for it
        let count = text.iter().filter(|byte| **byte == b'\n').count();
        if count == 0 {
            return LineIndex { breaks: &[], len };
        }
        let mut breaks = php_heap::ArenaVec::with_capacity_in(count, arena.bump());
        for (pos, byte) in text.iter().enumerate() {
            if *byte == b'\n' {
                breaks.push(u32::try_from(pos + 1).unwrap_or(u32::MAX));
            }
        }
        LineIndex {
            breaks: breaks.into_slice(),
            len,
        }
    }

    /// Number of lines (a trailing `\n` opens a final empty line).
    #[must_use]
    pub fn line_count(&self) -> u32 {
        u32::try_from(self.breaks.len() + 1).unwrap_or(u32::MAX)
    }

    /// The offset `line` (zero-based) starts at.
    fn start(&self, line: usize) -> Option<u32> {
        match line.checked_sub(1) {
            None => Some(0),
            Some(before) => self.breaks.get(before).copied(),
        }
    }

    /// Total indexed length in bytes.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// Whether the indexed text was empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The line and byte column of `offset`. An offset past the end clamps
    /// to the end, which is where an EOF diagnostic points.
    #[must_use]
    pub fn line_col(&self, offset: u32) -> LineCol {
        let offset = offset.min(self.len);
        // The first line starts at 0, at or before every offset, so the
        // breaks at or before it count the lines before its own.
        let line = self.breaks.partition_point(|&start| start <= offset);
        let col = offset - self.start(line).unwrap_or(0);
        LineCol {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            col,
        }
    }

    /// The 1-based line of `offset`, as php numbers lines in `on line N`:
    /// the `\n`s before it, plus one. An offset past the end is the last
    /// line, where an end-of-file diagnostic stands.
    #[must_use]
    pub fn line_number(&self, offset: u32) -> u32 {
        self.line_col(offset).line.saturating_add(1)
    }

    /// The byte offset of a [`LineCol`]; `None` when the line does not
    /// exist or the column exceeds its length.
    #[must_use]
    pub fn offset(&self, pos: LineCol) -> Option<u32> {
        let (start, end) = self.line_range(pos.line)?;
        let offset = start.checked_add(pos.col)?;
        (offset <= end).then_some(offset)
    }

    /// Byte range `(start, end)` of `line`, its `\n` included if present.
    #[must_use]
    pub fn line_range(&self, line: u32) -> Option<(u32, u32)> {
        let start = self.start(line as usize)?;
        let end = self.breaks.get(line as usize).copied().unwrap_or(self.len);
        Some((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lc(line: u32, col: u32) -> LineCol {
        LineCol { line, col }
    }

    #[test]
    fn ascii_lines() {
        let arena = crate::Arena::default();
        let idx = LineIndex::new(&arena, b"<?php\necho 1;\n");
        assert_eq!(idx.line_count(), 3); // trailing \n opens an empty line
        assert_eq!(idx.line_col(0), lc(0, 0));
        assert_eq!(idx.line_col(5), lc(0, 5)); // the \n itself
        assert_eq!(idx.line_col(6), lc(1, 0));
        assert_eq!(idx.line_col(13), lc(1, 7));
        assert_eq!(idx.line_col(14), lc(2, 0)); // EOF position
        assert_eq!(idx.offset(lc(1, 0)), Some(6));
        assert_eq!(idx.offset(lc(1, 99)), None);
        assert_eq!(idx.offset(lc(9, 0)), None);
    }

    #[test]
    fn crlf_keeps_cr_addressable() {
        let arena = crate::Arena::default();
        let idx = LineIndex::new(&arena, b"a\r\nb");
        assert_eq!(idx.line_col(1), lc(0, 1)); // the \r
        assert_eq!(idx.line_col(3), lc(1, 0));
    }

    #[test]
    fn offsets_past_end_clamp() {
        let arena = crate::Arena::default();
        let idx = LineIndex::new(&arena, b"ab");
        assert_eq!(idx.line_col(99), lc(0, 2));
    }

    #[test]
    fn high_bytes_are_one_column_each() {
        // "péh🐘x": é is 2 bytes, 🐘 is 4; a column is a byte count.
        let text = "p\u{e9}h\u{1F418}x";
        let arena = crate::Arena::default();
        let idx = LineIndex::new(&arena, text.as_bytes());
        assert_eq!(idx.line_col(3), lc(0, 3));
        assert_eq!(idx.line_col(8), lc(0, 8));
        assert_eq!(idx.line_col(9), lc(0, 9));
    }

    #[test]
    fn empty_file() {
        let arena = crate::Arena::default();
        let idx = LineIndex::new(&arena, b"");
        assert!(idx.is_empty());
        assert_eq!(idx.line_count(), 1);
        assert_eq!(idx.line_col(0), lc(0, 0));
        assert_eq!(idx.line_range(0), Some((0, 0)));
    }

    #[test]
    fn line_ranges() {
        let arena = crate::Arena::default();
        let idx = LineIndex::new(&arena, b"ab\ncd");
        assert_eq!(idx.line_range(0), Some((0, 3)));
        assert_eq!(idx.line_range(1), Some((3, 5)));
        assert_eq!(idx.line_range(2), None);
    }
}
