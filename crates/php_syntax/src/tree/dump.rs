//! The dump (design §4.9): the shape, one element per line, two spaces of
//! indentation per depth, `KIND@start..end` for a node and
//! `KIND@start..end "text"` for a token, which is Mahout's
//! `SyntaxNode::dump` format and what every snapshot fixture renders
//! through. The tree holds significant tokens only, so no whitespace or
//! comment is printed; the token text is the source's bytes, lossily
//! decoded and quoted as `Debug` quotes a string.

use std::fmt::Write as _;

use crate::lexer::Token;

use super::{Element, Tree};

/// The dump of `tree` over the `tokens` and `source` it was built from.
#[must_use]
pub fn dump(tree: &Tree, tokens: &[Token], source: &[u8]) -> String {
    let mut out = String::new();
    // The shape is a node followed by its subtree, so the depth of an
    // element is the number of enclosing subtrees still open, each held
    // as the position where it ends.
    let mut open: Vec<usize> = Vec::new();
    for (at, element) in tree.elements().iter().enumerate() {
        while open.last().is_some_and(|&end| end <= at) {
            open.pop();
        }
        let indent = open.len() * 2;
        match *element {
            Element::Node {
                kind,
                range,
                descendants,
                ..
            } => {
                let _ = writeln!(
                    out,
                    "{:indent$}{kind:?}@{}..{}",
                    "",
                    range.start,
                    range.end()
                );
                open.push(at + descendants as usize + 1);
            }
            Element::Token { kind, id } => {
                let (start, end, text) = match tokens.get(id.0 as usize) {
                    Some(token) => {
                        let start = token.offset as usize;
                        let end = start + token.len as usize;
                        (start, end, source.get(start..end).unwrap_or(&[]))
                    }
                    None => (0, 0, &[][..]),
                };
                let _ = writeln!(
                    out,
                    "{:indent$}{kind:?}@{start}..{end} {:?}",
                    "",
                    String::from_utf8_lossy(text)
                );
            }
        }
    }
    out
}
