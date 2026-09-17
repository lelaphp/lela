//! The contextual-keyword decision procedure (design §4.9). php's keyword
//! rules are where a hand-written parser rots, so every keyword-or-name
//! decision is made here and nowhere else, and `tests/contextual_keywords.rs`
//! enumerates them. The lexer keywordizes greedily and case-insensitively;
//! the parser decides by position and re-kinds a keyword consumed as a name
//! to `IDENT` through `bump_remap`, so the tree sees names, the token
//! stream keeps the lexer's view, and `token_get_all` still spells the
//! keyword.
//!
//! php-src: `zend_language_parser.y`'s `identifier`, `semi_reserved` and
//! `reserved_non_modifiers`, and the scanner's `enum` lookahead, reproduced
//! here as parser lookahead.

use crate::SyntaxKind as K;

/// Is `kind` usable as a member name: a method, a class constant, an enum
/// case, a property after `->`, a named-argument label?
///
/// php's `semi_reserved` covers the entire keyword set, modifiers and
/// `class` included (`const CLASS = 1` parses; its rejection is the
/// compiler's), and the magic constants beside it: `function __CLASS__()
/// {}` and `const __CLASS__ = …;` are legal members. `Foo::class` never
/// reaches member-name position; the scoped-access grammar reads `::class`
/// first.
pub(crate) fn is_member_name(kind: K) -> bool {
    kind == K::IDENT || kind.is_keyword() || kind == K::MAGIC_CONST
}

/// The keywords php does not reserve: the lexer keywordizes them, since the
/// schema declares them as keywords, but they are plain identifiers in any
/// position that is not their trigger.
///
/// - `enum` (8.1): a declaration only through [`enum_is_declaration`].
/// - `set` (8.4): a keyword only inside `private(set)` and as a property
///   accessor's name.
/// - `from`: a keyword only immediately after `yield`.
pub(crate) fn is_contextual_only(kind: K) -> bool {
    matches!(kind, K::ENUM_KW | K::SET_KW | K::FROM_KW)
}

/// Is the current token `callable`, the one keyword the lexer leaves an
/// `IDENT`? php's scanner keywordizes it (`T_CALLABLE`) and its grammar
/// takes it only as a type, and as a member name through
/// `semi_reserved`; a function's name (`T_STRING | T_READONLY`), a use
/// alias, a class-like name and a bare call are `T_STRING` alone, so
/// `function callable(){}`, `use A as callable;`, `class callable {}` and
/// `callable();` are parse errors there (probed 8.5.9). The lexer keeps
/// it an `IDENT` so `token_get_all` reads it by position, and the grammar
/// reads it by text at the sites php refuses.
pub(crate) fn is_callable_text(kind: K, text: &[u8]) -> bool {
    kind == K::IDENT && text.eq_ignore_ascii_case(b"callable")
}

/// The `enum` declaration lookahead, php's scanner rule as parser
/// lookahead: at `enum`, a declaration iff the next significant token
/// begins with a label byte, which is an identifier or any keyword but
/// `extends` and `implements` (`zend_language_scanner.l`: `"enum"
/// {WHITESPACE_OR_COMMENTS}[a-zA-Z_\x80-\xff]`). A class-like name takes
/// `T_STRING` in php, so `enum list {}` is `T_ENUM` and then a parse
/// error, `expecting identifier`, there and here; `enum(...)`, `enum::`,
/// `enum->` and `enum;` are expressions over an identifier named `enum`.
///
/// `next` is the token after the `enum` keyword.
pub(crate) fn enum_is_declaration(next: K) -> bool {
    next == K::IDENT
        || is_contextual_only(next)
        || (next.is_keyword() && !matches!(next, K::EXTENDS_KW | K::IMPLEMENTS_KW))
}

/// The `readonly` disambiguation (8.2): `readonly(` is a call of a
/// function named `readonly`, which php kept for compatibility; everywhere
/// else in a declaration position it is the modifier.
pub(crate) fn readonly_is_call(next: K) -> bool {
    next == K::L_PAREN
}

/// What a `static` token means in statement position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaticReading {
    /// `static $x …`, a function-scoped variable declaration.
    VarStmt,
    /// `static function` or `static fn`, a closure modifier: an expression.
    Closure,
    /// `static::…`, a class-reference expression.
    Expr,
}

/// Decides `static` in statement position from the token after it.
pub(crate) fn static_in_stmt(next: K) -> StaticReading {
    match next {
        K::VARIABLE => StaticReading::VarStmt,
        K::FUNCTION_KW | K::FN_KW => StaticReading::Closure,
        _ => StaticReading::Expr,
    }
}

/// The names of a property's accessors (8.4): `get` and `set` are
/// contextual identifiers; the lexer emits `SET_KW` for `set` and a plain
/// `IDENT` for `get`.
pub(crate) fn is_hook_name(kind: K) -> bool {
    kind == K::IDENT || kind == K::SET_KW
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_names_cover_all_keywords() {
        assert!(is_member_name(K::IDENT));
        assert!(is_member_name(K::LIST_KW));
        assert!(is_member_name(K::FOR_KW));
        assert!(is_member_name(K::CLASS_KW));
        assert!(is_member_name(K::MATCH_KW));
        assert!(!is_member_name(K::VARIABLE));
        assert!(!is_member_name(K::L_PAREN));
    }

    #[test]
    fn enum_lookahead() {
        assert!(enum_is_declaration(K::IDENT));
        // `enum Enum {}`: a contextual-only keyword is a remappable name.
        assert!(enum_is_declaration(K::ENUM_KW));
        // `enum list {}`: php's scanner reads `T_ENUM` before any label,
        // and the reserved word is then the parser's `expecting
        // identifier`; `enum extends` and `enum implements` stay names.
        assert!(enum_is_declaration(K::LIST_KW));
        assert!(!enum_is_declaration(K::EXTENDS_KW));
        assert!(!enum_is_declaration(K::IMPLEMENTS_KW));
        assert!(!enum_is_declaration(K::L_PAREN));
        assert!(!enum_is_declaration(K::COLON_COLON));
        assert!(!enum_is_declaration(K::SEMICOLON));
    }

    #[test]
    fn static_readings() {
        assert_eq!(static_in_stmt(K::VARIABLE), StaticReading::VarStmt);
        assert_eq!(static_in_stmt(K::FUNCTION_KW), StaticReading::Closure);
        assert_eq!(static_in_stmt(K::FN_KW), StaticReading::Closure);
        assert_eq!(static_in_stmt(K::COLON_COLON), StaticReading::Expr);
    }
}
