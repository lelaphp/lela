//! What `TOKEN_PARSE` changes. php's `tokenize_parse`
//! (`Z/ext/tokenizer/tokenizer.c:455`) runs the parser over the scanner
//! with `on_event` collecting the stream, and the grammar reports back
//! through `zend_lex_tstring` (`Z/Zend/zend_language_scanner.l:300`)
//! wherever it takes a keyword as a name: `identifier: semi_reserved`,
//! which is a method, a class constant, an enum case, a named argument,
//! a member after `::`, a trait alias and a namespace's name; the
//! `class_const_decl` and `trait_alias` rules that spell `semi_reserved`
//! and `reserved_non_modifiers` themselves; and `T_READONLY` as a
//! function's name or a call's. What comes back is `ON_FEEDBACK` with
//! `T_STRING`, and the tokenizer writes it onto the token.
//!
//! Here what comes back is the tree's shape: a keyword the grammar
//! consumed as a name is `IDENT` in the tree where the stream keeps the
//! keyword (`php_syntax::tree::Element`). The grammar remaps more widely
//! than php reports, `static` as a class reference and `array` in a
//! type among them, so a remap counts only inside the nodes php's sites
//! build, `MEMBER_NAME`, `NAMED_ARG` and `NAMESPACE_DEF`, and for
//! `readonly` anywhere. `callable` is the one keyword the lexer leaves a
//! name, so it is read by position alone, and `::class`, which the
//! grammar reads as a selector without remapping it, is php's
//! `identifier` too.

use php_ext::syntax::{Element, Parsed, SyntaxKind};

/// The start offset of every token php's parser reports as `T_STRING`,
/// in stream order.
pub(super) fn identifiers(parsed: &Parsed) -> Vec<usize> {
    use SyntaxKind as K;
    let mut fed = Vec::new();
    // The nodes the walk is inside, innermost last, each with the index
    // of the last element of its subtree.
    let mut open: Vec<(SyntaxKind, usize)> = Vec::new();
    let mut previous = None;
    for (at, element) in parsed.tree.elements().iter().enumerate() {
        while open.last().is_some_and(|(_, end)| *end < at) {
            open.pop();
        }
        match *element {
            Element::Node {
                kind, descendants, ..
            } => open.push((kind, at + descendants as usize)),
            Element::Token { kind, id } => {
                let Some(token) = parsed.tokens.get(id.0 as usize) else {
                    continue;
                };
                let start = token.offset as usize;
                let text = parsed
                    .source
                    .get(start..start + token.len as usize)
                    .unwrap_or(&[]);
                let innermost = open.last().map(|(kind, _)| *kind);
                let named = kind == K::IDENT
                    && (token.kind != K::IDENT || text.eq_ignore_ascii_case(b"callable"));
                let site = matches!(
                    innermost,
                    Some(K::MEMBER_NAME | K::NAMED_ARG | K::NAMESPACE_DEF)
                ) || token.kind == K::READONLY_KW;
                let selector = kind == K::CLASS_KW && previous == Some(K::COLON_COLON);
                if (named && site) || selector {
                    fed.push(start);
                }
                previous = Some(kind);
            }
        }
    }
    fed
}
