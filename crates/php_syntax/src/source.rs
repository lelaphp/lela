//! The source model under the lexer: one file's bytes, byte spans into it,
//! and the diagnostic the lexer raises. Files are bytes, nothing here
//! assumes UTF-8, and offsets are `u32`, which caps a file at 4 GiB; the
//! cap is held at [`load_bytes`] with a diagnostic, never by truncation.

/// Identifies a file. A newtype over `u32`; the mapping from a `FileId` to
/// a path is its owner's, not this type's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

/// A byte range inside a single file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    /// File the range points into.
    pub file: FileId,
    /// Byte offset of the first byte of the range.
    pub start: u32,
    /// Length of the range in bytes (may be zero).
    pub len: u32,
}

/// A byte range without a file: positions inside one syntax tree.
///
/// [`Span`] is a `TextRange` and a [`FileId`]; a tree does not know its
/// file, so the tree traffics in `TextRange` and a diagnostic adds the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TextRange {
    /// Byte offset of the first byte.
    pub start: u32,
    /// Length in bytes (may be zero).
    pub len: u32,
}

impl TextRange {
    /// The range `start..end`.
    #[must_use]
    pub fn new(start: u32, end: u32) -> TextRange {
        debug_assert!(end >= start, "range end before start ({start}..{end})");
        TextRange {
            start,
            // Saturate in release: a backwards range is an empty one, not a
            // wrapped ~4 GiB one.
            len: end.saturating_sub(start),
        }
    }

    /// One past the last byte.
    #[must_use]
    pub fn end(self) -> u32 {
        self.start + self.len
    }

    /// Does this range contain `offset`?
    #[must_use]
    pub fn contains(self, offset: u32) -> bool {
        self.start <= offset && offset < self.end()
    }

    /// Does this range fully contain `other`?
    #[must_use]
    pub fn contains_range(self, other: TextRange) -> bool {
        self.start <= other.start && other.end() <= self.end()
    }

    /// Attaches a file, producing a [`Span`].
    #[must_use]
    pub fn in_file(self, file: FileId) -> Span {
        Span {
            file,
            start: self.start,
            len: self.len,
        }
    }
}

impl std::fmt::Display for TextRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}", self.start, self.end())
    }
}

impl Span {
    /// A span covering `start..end` in `file`.
    #[must_use]
    pub fn new(file: FileId, start: u32, end: u32) -> Span {
        debug_assert!(end >= start, "span end before start ({start}..{end})");
        Span {
            file,
            start,
            len: end.saturating_sub(start),
        }
    }

    /// Byte offset one past the end of the range.
    #[must_use]
    pub fn end(self) -> u32 {
        self.start + self.len
    }

    /// The file-less range of this span.
    #[must_use]
    pub fn range(self) -> TextRange {
        TextRange {
            start: self.start,
            len: self.len,
        }
    }

    /// Does this span contain the byte at `offset`?
    #[must_use]
    pub fn contains(self, offset: u32) -> bool {
        self.start <= offset && offset < self.end()
    }

    /// The smallest span covering both `self` and `other`, which point into
    /// the same file.
    #[must_use]
    pub fn cover(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file, "cover() across files");
        let start = self.start.min(other.start);
        let end = self.end().max(other.end());
        Span::new(self.file, start, end)
    }
}

/// The largest loadable file, in bytes: 4 GiB less one, so every in-file
/// offset, the one-past-the-end EOF position included, fits a `u32`.
pub const MAX_FILE_LEN: u64 = u32::MAX as u64;

/// Turns raw file contents into the retained source, holding the 4 GiB
/// span-addressing cap. A file over the cap is refused with the diagnostic
/// that says so; its contents are never truncated.
pub fn load_bytes(file: FileId, bytes: Vec<u8>) -> Result<Box<[u8]>, Diagnostic> {
    if bytes.len() as u64 > MAX_FILE_LEN {
        let gib = f64::from(u32::MAX) / (1024.0 * 1024.0 * 1024.0);
        return Err(Diagnostic::new(
            Span {
                file,
                start: 0,
                len: 0,
            },
            format!(
                "file is {} bytes, larger than the {gib:.0} GiB limit",
                bytes.len()
            ),
        ));
    }
    Ok(bytes.into_boxed_slice())
}

/// One diagnostic the lexer raised: what php's scanner says where it has a
/// text of its own (`Unterminated comment starting line 3`,
/// `Invalid numeric literal`) and the span it points at. There is no code number
/// (design §6.1). Where php reports the same defect from its parser, with
/// bison's `unexpected …, expecting …` for the state it was in, the text
/// here is still the inherited one; the sites are re-worded to php's, and
/// the shape settled, with the parse entry (#206).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The defect site: the smallest range that must change.
    pub span: Span,
    /// The text, php's where php's scanner has one.
    pub message: String,
}

impl Diagnostic {
    /// A diagnostic at `span` with `message`.
    #[must_use]
    pub fn new(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_end() {
        let span = Span {
            file: FileId(0),
            start: 10,
            len: 5,
        };
        assert_eq!(span.end(), 15);
        assert!(span.contains(10));
        assert!(span.contains(14));
        assert!(!span.contains(15));
    }

    #[test]
    fn span_cover() {
        let a = Span::new(FileId(0), 2, 5);
        let b = Span::new(FileId(0), 9, 12);
        let c = a.cover(b);
        assert_eq!((c.start, c.end()), (2, 12));
    }

    #[test]
    fn load_bytes_small_ok() {
        let source = load_bytes(FileId(0), b"<?php\n".to_vec()).map(|s| s.to_vec());
        assert_eq!(source, Ok(b"<?php\n".to_vec()));
    }
}
