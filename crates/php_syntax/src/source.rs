//! The source model under the lexer: one file's bytes, byte spans into it,
//! and the diagnostic the lexer raises. Files are bytes, nothing here
//! assumes UTF-8, and offsets are `u32`, which caps a file at 4 GiB; the
//! cap is held at [`load_bytes`] with a diagnostic, never by truncation.

/// Identifies a file. A newtype over `u32`; the mapping from a `FileId` to
/// a path is its owner's, not this type's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
            1,
            format!(
                "file is {} bytes, larger than the {gib:.0} GiB limit",
                bytes.len()
            ),
        ));
    }
    Ok(bytes.into_boxed_slice())
}

/// php's kind for what the lexer or the parser raises: the `E_*` level
/// `php_error_cb` renders it under, which is the head of the line php
/// prints (`Parse error`, `Fatal error`, `Warning`, `Deprecated`) and
/// whether the request goes on. What design §6.1 calls the error kind; a
/// diagnostic carries no code number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Level {
    /// `E_PARSE`: what the scanner and the parser refuse with. The
    /// request ends before it runs.
    Parse,
    /// `E_COMPILE_ERROR`: a fatal php raises while compiling the tree, so
    /// after every parse error in the file; the parser's one such is the
    /// `(unset)` cast.
    CompileError,
    /// `E_COMPILE_WARNING`: the request goes on; the scanner's octal
    /// escape overflow.
    CompileWarning,
    /// `E_DEPRECATED`: the request goes on; the scanner's non-canonical
    /// cast spelling.
    Deprecated,
}

impl Level {
    /// Whether php stops on it: the request never runs after a `Parse` or
    /// a `CompileError`.
    #[must_use]
    pub fn is_fatal(self) -> bool {
        matches!(self, Level::Parse | Level::CompileError)
    }
}

/// One diagnostic the lexer or the parser raised: php's text, at php's
/// line, under php's level (design §6.1). The text is what `php` prints
/// between the level's head and ` in <file> on line <n>`: bison's
/// `syntax error, unexpected …, expecting …` for the state the parser
/// was in, the scanner's own sentences (`Invalid numeric literal`,
/// `Unterminated comment starting line 3`), and the bracket balance
/// (`Unclosed '{' on line 3`). There is no code number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// The defect site: the smallest range that must change.
    pub span: Span,
    /// The 1-based line php reports, `CG(zend_lineno)` when it raised:
    /// the line the scanner stands on after the unexpected token, which
    /// is the token's last line, and the line the scanner had reached
    /// where it throws mid-token.
    pub line: u32,
    /// php's level for it.
    pub level: Level,
    /// php's text.
    pub message: String,
}

impl Diagnostic {
    /// A parse error at `span`, reported on `line`, with `message`.
    #[must_use]
    pub fn new(span: Span, line: u32, message: impl Into<String>) -> Diagnostic {
        Diagnostic::of(Level::Parse, span, line, message)
    }

    /// A diagnostic under `level` at `span`, reported on `line`.
    #[must_use]
    pub fn of(level: Level, span: Span, line: u32, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            line,
            level,
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
