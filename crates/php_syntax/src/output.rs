//! php's token filter (`SCNG(output_filter)`, design §4.9): under
//! `zend.multibyte`, where the file's encoding and the internal one
//! differ and the internal one is not the lexer's, the scanner converts
//! what it hands its parser rather than the file it scans. php's
//! scanner does it as it makes each token's zval: `zend_copy_value` for
//! `T_STRING`, `T_VARIABLE`, the three `T_NAME_*`, `T_STRING_VARNAME`
//! and `T_INLINE_HTML`, and the tails of `zend_scan_escape_string` and
//! the `'…'` rule for a string's bytes once its escapes are read
//! (`Z/Zend/zend_language_scanner.l:916-928, 1137-1146, 2372-2381,
//! 2613-2624`). A nowdoc's text and a `T_NUM_STRING` are copied as they
//! stand. [`filter_tokens`] passes the filter once over every such
//! token of a parse and keeps what it made in [`Filtered`], which
//! [`token_text`] and [`TokenValue::of`] answer from, so the compiler
//! reads a filtered name or string the way it reads an unfiltered one.

use crate::tree::{
    HeredocExpr, InterpolatedStringExpr, LiteralExpr, ShellExecExpr, StringPart, TokenId, Tree,
};
use crate::value::Holder;
use crate::{Parsed, SyntaxKind, TokenValue};

/// What the token filter made of each token it converted, by the
/// token's index: a name's whole text, its `$`, `\` or `namespace\`
/// prefix kept as written and the rest converted, or a string's bytes
/// once its escapes are read, in the order of the tokens. Empty where no
/// filter stands, which a parse gives up with nothing to walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filtered(Vec<(u32, Box<[u8]>)>);

impl Filtered {
    /// What the filter made of `token`, where it converted it.
    #[must_use]
    pub fn get(&self, token: TokenId) -> Option<&[u8]> {
        let at = self
            .0
            .binary_search_by_key(&token.0, |(index, _)| *index)
            .ok()?;
        self.0.get(at).map(|(_, bytes)| &bytes[..])
    }
}

/// How many bytes of a name token's text php's `RETURN_TOKEN_WITH_STR`
/// leaves out of the value it copies, the `offset` its rule passes:
/// `T_VARIABLE`'s `$`, `T_NAME_FULLY_QUALIFIED`'s `\` and
/// `T_NAME_RELATIVE`'s `namespace\`; none for a token php copies whole,
/// and `None` for a token it hands over as it stands.
fn copied_from(kind: SyntaxKind) -> Option<usize> {
    match kind {
        SyntaxKind::IDENT
        | SyntaxKind::MEMBER_IDENT
        | SyntaxKind::NAME_QUALIFIED
        | SyntaxKind::INLINE_HTML => Some(0),
        SyntaxKind::VARIABLE | SyntaxKind::NAME_FULLY_QUALIFIED => Some(1),
        SyntaxKind::NAME_RELATIVE => Some(b"namespace\\".len()),
        _ => None,
    }
}

/// A token's text as the scanner handed it over: what the token filter
/// made of a name ([`filter_tokens`]), and the source's bytes for every
/// other token and where no filter stands.
#[must_use]
pub fn token_text<'p>(parsed: &'p Parsed<'_>, id: TokenId) -> &'p [u8] {
    if let Some(filtered) = parsed.filtered.get(id) {
        return filtered;
    }
    let Some(token) = parsed.tokens.get(id.0 as usize) else {
        return &[];
    };
    parsed
        .source
        .get(token.offset as usize..(token.offset + token.len) as usize)
        .unwrap_or_default()
}

impl Parsed<'_> {
    /// Pass the token filter over the parse, once per token php's scanner
    /// hands through it, and keep what it made ([`Filtered`]).
    fn filter_tokens(&mut self, filter: &mut dyn FnMut(&[u8]) -> Vec<u8>) {
        self.filtered = Filtered::default();
        let mut made = Vec::new();
        for (index, token) in self.tokens.iter().enumerate() {
            let Some(from) = copied_from(token.kind) else {
                continue;
            };
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            let text = token_text(self, TokenId(index));
            let from = from.min(text.len());
            let mut converted = text[..from].to_vec();
            converted.extend(filter(&text[from..]));
            made.push((index, converted.into_boxed_slice()));
        }
        let tree = &self.tree;
        let mut strings: Vec<(Holder<'_>, TokenId)> = Vec::new();
        for node in tree.nodes::<LiteralExpr>() {
            if let Some(value) = node.value
                && self.tokens.get(value.0 as usize).map(|t| t.kind)
                    == Some(SyntaxKind::SINGLE_QUOTED)
            {
                strings.push((Holder::Scalar(node), value));
            }
        }
        for node in tree.nodes::<InterpolatedStringExpr>() {
            parts(
                tree,
                Holder::DoubleQuoted(node),
                tree.list(node.parts),
                &mut strings,
            );
        }
        for node in tree.nodes::<HeredocExpr>() {
            parts(
                tree,
                Holder::Heredoc(node),
                tree.list(node.parts),
                &mut strings,
            );
        }
        for node in tree.nodes::<ShellExecExpr>() {
            parts(
                tree,
                Holder::ShellExec(node),
                tree.list(node.parts),
                &mut strings,
            );
        }
        for (holder, token) in strings {
            if let Some(TokenValue::Bytes(bytes)) = TokenValue::of(self, holder, token) {
                made.push((token.0, filter(&bytes).into_boxed_slice()));
            }
        }
        // A token made twice keeps what it was made last, as a map would.
        made.sort_by_key(|(index, _)| *index);
        made.reverse();
        made.dedup_by_key(|(index, _)| *index);
        made.reverse();
        self.filtered = Filtered(made);
    }
}

/// Each written part of a string `holder` holds, with the holder.
fn parts<'a>(
    tree: &Tree<'_>,
    holder: Holder<'a>,
    parts: &[StringPart],
    into: &mut Vec<(Holder<'a>, TokenId)>,
) {
    for part in parts {
        if let StringPart::LiteralStringPart(id) = part
            && let Some(text) = tree[*id].text
        {
            into.push((holder, text));
        }
    }
}

/// Pass the token filter over `parsed`, once per token php's scanner
/// hands through it, and keep what it made ([`Filtered`]).
pub fn filter_tokens(parsed: &mut Parsed<'_>, filter: &mut dyn FnMut(&[u8]) -> Vec<u8>) {
    parsed.filter_tokens(filter);
}
