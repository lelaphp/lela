//! php's `T_*` names for the lexer's kinds: the rename tables, and nothing
//! but the tables. No scanning lives here; the lexer is the source of truth
//! and this file only spells its kinds the way php's `token_get_all` does.
//! Its readers are `ext/tokenizer`, which is `token_get_all` here, and the
//! token differential the harness will hold against a live php, so a
//! php-side rename is one edit rather than two.
//!
//! `keyword_name` is deliberately not here. `ext/tokenizer` intercepts
//! `PRIVATE_KW`, `PROTECTED_KW`, `PUBLIC_KW` and `YIELD_KW` before any
//! lookup (the `(set)` fusion and the `yield from` fuse), so its table
//! omits those four rows on purpose, and the differential's does not. One
//! table serving both would carry rows unreachable on one side, an answer
//! that reads as agreement without being one; each copy stays where its
//! interception is. `T_BAD_CHARACTER` stays with `ext/tokenizer` for the
//! same reason: the differential never sees a rejected file.
//!
//! The tables are held to the oracle's names by `php_names` in the lexer's
//! tests: one row per table lexed here and spelled through the table its
//! kind selects, against the five lines `token_get_all` prints for the same
//! rows under the pinned php. The witness that holds them to the same
//! lines under the instrument waits on `token_get_all` and `token_name`
//! as natives of `ext/tokenizer` (#224), since `xtask witness` serves a
//! snippet through the engine, and lands beside these tables when they are.

use crate::SyntaxKind;

/// php's name for a multi-character operator; nothing for any other kind,
/// a single-character operator included, which `token_get_all` answers as
/// the character itself.
#[must_use]
pub fn operator_name(kind: SyntaxKind) -> Option<&'static str> {
    use SyntaxKind as K;
    Some(match kind {
        K::AMP_AMP => "T_BOOLEAN_AND",
        K::AMP_EQ => "T_AND_EQUAL",
        K::ARROW => "T_OBJECT_OPERATOR",
        K::ATTRIBUTE_OPEN => "T_ATTRIBUTE",
        K::BACKSLASH => "T_NS_SEPARATOR",
        // `!=` and `<>` are one kind in php.
        K::BANG_EQ | K::LT_GT => "T_IS_NOT_EQUAL",
        K::BANG_EQ_EQ => "T_IS_NOT_IDENTICAL",
        K::CARET_EQ => "T_XOR_EQUAL",
        K::COLON_COLON => "T_DOUBLE_COLON",
        K::DOLLAR_L_CURLY => "T_DOLLAR_OPEN_CURLY_BRACES",
        K::DOT_DOT_DOT => "T_ELLIPSIS",
        K::DOT_EQ => "T_CONCAT_EQUAL",
        K::EQ_EQ => "T_IS_EQUAL",
        K::EQ_EQ_EQ => "T_IS_IDENTICAL",
        K::FAT_ARROW => "T_DOUBLE_ARROW",
        K::GT_EQ => "T_IS_GREATER_OR_EQUAL",
        K::LT_EQ => "T_IS_SMALLER_OR_EQUAL",
        K::MINUS_EQ => "T_MINUS_EQUAL",
        K::MINUS_MINUS => "T_DEC",
        K::NULLSAFE_ARROW => "T_NULLSAFE_OBJECT_OPERATOR",
        K::PERCENT_EQ => "T_MOD_EQUAL",
        K::PIPE_EQ => "T_OR_EQUAL",
        K::PIPE_GT => "T_PIPE",
        K::PIPE_PIPE => "T_BOOLEAN_OR",
        K::PLUS_EQ => "T_PLUS_EQUAL",
        K::PLUS_PLUS => "T_INC",
        K::QUESTION_QUESTION => "T_COALESCE",
        K::QUESTION_QUESTION_EQ => "T_COALESCE_EQUAL",
        K::SHL => "T_SL",
        K::SHL_EQ => "T_SL_EQUAL",
        K::SHR => "T_SR",
        K::SHR_EQ => "T_SR_EQUAL",
        K::SLASH_EQ => "T_DIV_EQUAL",
        K::SPACESHIP => "T_SPACESHIP",
        K::STAR_EQ => "T_MUL_EQUAL",
        K::STAR_STAR => "T_POW",
        K::STAR_STAR_EQ => "T_POW_EQUAL",
        _ => return None,
    })
}

/// php's name for a magic constant, from its spelling, which the lexer
/// matched without regard to case.
#[must_use]
pub fn magic_const_name(text: &[u8]) -> &'static str {
    match text.to_ascii_lowercase().as_slice() {
        b"__line__" => "T_LINE",
        b"__file__" => "T_FILE",
        b"__dir__" => "T_DIR",
        b"__function__" => "T_FUNC_C",
        b"__class__" => "T_CLASS_C",
        b"__trait__" => "T_TRAIT_C",
        b"__method__" => "T_METHOD_C",
        b"__namespace__" => "T_NS_C",
        b"__property__" => "T_PROPERTY_C",
        _ => "T_STRING", // unreachable for a lexer-emitted MAGIC_CONST
    }
}

/// php's cast token name from the cast's interior word, the parentheses and
/// the blanks inside them dropped. The same for `CAST_REMOVED`: php still
/// lexes `(real)` as `T_DOUBLE_CAST` and `(unset)` as `T_UNSET_CAST`, and
/// refuses them in the compiler, not the scanner.
#[must_use]
pub fn cast_name(text: &[u8]) -> &'static str {
    let inner: Vec<u8> = text
        .iter()
        .copied()
        .filter(|b| !matches!(b, b'(' | b')' | b' ' | b'\t'))
        .collect();
    match inner.to_ascii_lowercase().as_slice() {
        b"void" => "T_VOID_CAST",
        b"int" | b"integer" => "T_INT_CAST",
        b"bool" | b"boolean" => "T_BOOL_CAST",
        b"float" | b"double" | b"real" => "T_DOUBLE_CAST",
        b"string" | b"binary" => "T_STRING_CAST",
        b"array" => "T_ARRAY_CAST",
        b"object" => "T_OBJECT_CAST",
        b"unset" => "T_UNSET_CAST",
        _ => "T_STRING", // unreachable for a lexer-emitted CAST
    }
}

/// Does the text of an `INT` overflow php's 64-bit long? The lexer records
/// the digits and nothing else; the reader owns the promotion of an
/// overflowing `INT` to `T_DNUMBER`, which is where php's scanner makes it
/// a double.
#[must_use]
pub fn int_overflows(text: &[u8]) -> bool {
    let digits: Vec<u8> = text.iter().copied().filter(|&b| b != b'_').collect();
    let (radix, body): (u32, &[u8]) = match digits.as_slice() {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        [b'0', b'b' | b'B', rest @ ..] => (2, rest),
        [b'0', b'o' | b'O', rest @ ..] => (8, rest),
        // Legacy octal: a leading 0 and more digits. A bad digit (`08`) is
        // the lexer's diagnostic; the prefix is read conservatively here.
        [b'0', rest @ ..] if !rest.is_empty() => (8, rest),
        all => (10, all),
    };
    let mut value: u128 = 0;
    for &b in body {
        let Some(d) = char::from(b).to_digit(radix) else {
            return false; // a bad digit is the lexer's diagnostic, not a promotion
        };
        value = match value
            .checked_mul(u128::from(radix))
            .and_then(|v| v.checked_add(u128::from(d)))
        {
            Some(v) => v,
            None => return true,
        };
    }
    value > u128::from(i64::MAX.unsigned_abs())
}

/// The 8.1 ampersand split, recomputed from the bytes after the `&`, at
/// `after`; the lexer's one `AMP` is 1:1 with the pair by construction.
/// Zend: `"&"{TABS_AND_SPACES}("$"|"...")`, and its lookahead skips
/// newlines and comments too (`A & /* c */ $p` is followed; php-src's
/// `intersection_types/parsing_comment.php`).
#[must_use]
pub fn amp_name(text: &[u8], after: usize) -> &'static str {
    let mut i = after;
    loop {
        match text.get(i) {
            Some(b' ' | b'\t' | b'\r' | b'\n') => i += 1,
            Some(b'/') if text.get(i + 1) == Some(&b'*') => {
                let close = text
                    .get(i + 2..)
                    .and_then(|rest| rest.windows(2).position(|w| w == b"*/"))
                    .map(|at| i + 2 + at + 2);
                match close {
                    Some(end) => i = end,
                    None => break,
                }
            }
            Some(b'/') if text.get(i + 1) == Some(&b'/') => {
                while !matches!(text.get(i), None | Some(b'\n' | b'\r')) {
                    i += 1;
                }
            }
            Some(b'#') if text.get(i + 1) != Some(&b'[') => {
                while !matches!(text.get(i), None | Some(b'\n' | b'\r')) {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    let rest = text.get(i..).unwrap_or(&[]);
    if rest.first() == Some(&b'$') || rest.starts_with(b"...") {
        "T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG"
    } else {
        "T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG"
    }
}
