//! The cursor over the significant tokens: the grammar addresses the
//! `n`-th significant token and never sees whitespace, a comment or a
//! heredoc's indentation, which stay in the lossless stream for the
//! builder to pass over (design §4.9). Open and close tags, inline HTML
//! and the bytes after `__halt_compiler` are significant: the statement
//! grammar reads them, a close tag standing in for `;`.

use crate::SyntaxKind;
use crate::lexer::Token;
use crate::source::TextRange;

/// The lexed stream with its significant tokens indexed.
pub struct Input<'t> {
    text: &'t [u8],
    tokens: &'t [Token],
    /// The index into `tokens` of each significant token.
    significant: Vec<u32>,
}

impl<'t> Input<'t> {
    /// Index `tokens`, the lexer's stream over `text`, whose texts
    /// concatenate to it.
    #[must_use]
    #[cfg(test)]
    pub fn new(text: &'t [u8], tokens: &'t [Token]) -> Input<'t> {
        Input::new_in(text, tokens, Vec::new())
    }

    /// [`Input::new`] into the room `significant` lends, which
    /// [`Input::into_significant`] hands back.
    pub(crate) fn new_in(
        text: &'t [u8],
        tokens: &'t [Token],
        mut significant: Vec<u32>,
    ) -> Input<'t> {
        significant.clear();
        significant.reserve(tokens.len());
        significant.extend(
            tokens
                .iter()
                .enumerate()
                .filter(|(_, token)| !token.kind.is_trivia())
                .map(|(i, _)| u32::try_from(i).unwrap_or(u32::MAX)),
        );
        Input {
            text,
            tokens,
            significant,
        }
    }

    /// The significant-token table's room, emptied.
    pub(crate) fn into_significant(self) -> Vec<u32> {
        let mut significant = self.significant;
        significant.clear();
        significant
    }

    /// The number of significant tokens.
    #[must_use]
    pub fn len(&self) -> usize {
        self.significant.len()
    }

    /// Whether there is no significant token.
    #[must_use]
    // `len` has readers and clippy wants the pair.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.significant.is_empty()
    }

    fn token(&self, n: usize) -> Option<&'t Token> {
        self.significant
            .get(n)
            .and_then(|&raw| self.tokens.get(raw as usize))
    }

    /// The kind of the `n`-th significant token; `EOF` past the end.
    #[must_use]
    pub fn kind(&self, n: usize) -> SyntaxKind {
        self.token(n).map_or(SyntaxKind::EOF, |token| token.kind)
    }

    /// The source bytes of the `n`-th significant token; empty past the
    /// end.
    #[must_use]
    pub fn text(&self, n: usize) -> &'t [u8] {
        self.token(n).map_or(&[], |token| {
            let start = token.offset as usize;
            self.text
                .get(start..start + token.len as usize)
                .unwrap_or(&[])
        })
    }

    /// The byte range of the `n`-th significant token; zero-width at the
    /// end of the source past the end.
    #[must_use]
    pub fn range(&self, n: usize) -> TextRange {
        self.token(n).map_or_else(
            || {
                let end = u32::try_from(self.text.len()).unwrap_or(u32::MAX);
                TextRange { start: end, len: 0 }
            },
            |token| TextRange {
                start: token.offset,
                len: token.len,
            },
        )
    }

    /// Whether the whitespace or comments between significant tokens
    /// `n - 1` and `n` hold a newline: what `?>` eating the newline after
    /// it and the statement glue read.
    #[must_use]
    pub fn newline_before(&self, n: usize) -> bool {
        let end_raw = self
            .significant
            .get(n)
            .map_or(self.tokens.len(), |&raw| raw as usize);
        let start_raw = n
            .checked_sub(1)
            .and_then(|p| self.significant.get(p))
            .map_or(0, |&raw| raw as usize + 1);
        self.tokens
            .get(start_raw..end_raw)
            .unwrap_or(&[])
            .iter()
            .any(|token| {
                let start = token.offset as usize;
                token.kind.is_trivia()
                    && self
                        .text
                        .get(start..start + token.len as usize)
                        .is_some_and(|text| text.contains(&b'\n'))
            })
    }

    /// The source text.
    #[must_use]
    pub fn source(&self) -> &'t [u8] {
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(kinds: &[(SyntaxKind, u32)]) -> Vec<Token> {
        let mut offset = 0;
        kinds
            .iter()
            .map(|&(kind, len)| {
                let token = Token { kind, offset, len };
                offset += len;
                token
            })
            .collect()
    }

    #[test]
    fn significant_skips_whitespace() {
        // `$x = 1;` lexed: VARIABLE WS EQ WS INT SEMICOLON
        let text = b"$x = 1;";
        let raw = tokens(&[
            (SyntaxKind::VARIABLE, 2),
            (SyntaxKind::WHITESPACE, 1),
            (SyntaxKind::EQ, 1),
            (SyntaxKind::WHITESPACE, 1),
            (SyntaxKind::INT, 1),
            (SyntaxKind::SEMICOLON, 1),
        ]);
        let input = Input::new(text, &raw);
        assert_eq!(input.len(), 4);
        assert_eq!(input.kind(0), SyntaxKind::VARIABLE);
        assert_eq!(input.text(0), b"$x");
        assert_eq!(input.kind(2), SyntaxKind::INT);
        assert_eq!(input.range(2), TextRange { start: 5, len: 1 });
        assert_eq!(input.kind(4), SyntaxKind::EOF);
        assert_eq!(input.range(4), TextRange { start: 7, len: 0 });
        assert_eq!(input.text(4), b"");
    }

    #[test]
    fn newline_detection() {
        let text = b"$x\n=1";
        let raw = tokens(&[
            (SyntaxKind::VARIABLE, 2),
            (SyntaxKind::WHITESPACE, 1),
            (SyntaxKind::EQ, 1),
            (SyntaxKind::INT, 1),
        ]);
        let input = Input::new(text, &raw);
        assert!(input.newline_before(1));
        assert!(!input.newline_before(2));
        assert!(!input.newline_before(0));
    }
}
