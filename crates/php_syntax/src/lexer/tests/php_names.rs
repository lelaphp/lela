//! The five tables of `php_names`, held to the five lines the oracle's
//! `token_get_all` prints for five rows, one per table: the same rows lexed
//! here, each token spelled through the table its kind selects. The lines
//! were read from the pinned php at 8.5.9 once and are carried here as
//! `PINNED`; the witness in `php_names` holds one token per table each
//! time the instrument reads it, through `ext/tokenizer`'s `token_get_all`. `#[`
//! stands first in the operators row because `-> #[` is a comment to php:
//! `ST_LOOKING_FOR_PROPERTY` reads the `#`.

use super::*;
use crate::{amp_name, cast_name, int_overflows, magic_const_name, operator_name};

/// The rows, one per table; the operators row ends in `"${x}"` since `${`
/// opens only inside a string.
const ROWS: [&str; 5] = [
    r#"#[ && &= -> \ != <> !== ^= :: ... .= == === => >= <= -= -- ?-> %= |= |> || += ++ ?? ??= << <<= >> >>= /= <=> *= ** **= "${x}""#,
    "__LINE__ __FILE__ __DIR__ __FUNCTION__ __CLASS__ __TRAIT__ __METHOD__ __NAMESPACE__ __PROPERTY__ __Line__",
    "(void) (int) (integer) (bool) ( boolean ) (float) (double) (real) (string) (binary) (array) (object) (unset) (INT)",
    "9223372036854775807 9223372036854775808 -9223372036854775808 0x7FFFFFFFFFFFFFFF 0x8000000000000000 0b111 0777 01777777777777777777777 0o17 1_000 00",
    "& $x &...$y A&B & /* c */ $p A & B A&/* c */$p A & $x &",
];

/// What the oracle printed for each row.
const PINNED: [&str; 5] = [
    "T_ATTRIBUTE T_BOOLEAN_AND T_AND_EQUAL T_OBJECT_OPERATOR T_NS_SEPARATOR T_IS_NOT_EQUAL T_IS_NOT_EQUAL T_IS_NOT_IDENTICAL T_XOR_EQUAL T_DOUBLE_COLON T_ELLIPSIS T_CONCAT_EQUAL T_IS_EQUAL T_IS_IDENTICAL T_DOUBLE_ARROW T_IS_GREATER_OR_EQUAL T_IS_SMALLER_OR_EQUAL T_MINUS_EQUAL T_DEC T_NULLSAFE_OBJECT_OPERATOR T_MOD_EQUAL T_OR_EQUAL T_PIPE T_BOOLEAN_OR T_PLUS_EQUAL T_INC T_COALESCE T_COALESCE_EQUAL T_SL T_SL_EQUAL T_SR T_SR_EQUAL T_DIV_EQUAL T_SPACESHIP T_MUL_EQUAL T_POW T_POW_EQUAL \" T_DOLLAR_OPEN_CURLY_BRACES T_STRING_VARNAME } \"",
    "T_LINE T_FILE T_DIR T_FUNC_C T_CLASS_C T_TRAIT_C T_METHOD_C T_NS_C T_PROPERTY_C T_LINE",
    "T_VOID_CAST T_INT_CAST T_INT_CAST T_BOOL_CAST T_BOOL_CAST T_DOUBLE_CAST T_DOUBLE_CAST T_DOUBLE_CAST T_STRING_CAST T_STRING_CAST T_ARRAY_CAST T_OBJECT_CAST T_UNSET_CAST T_INT_CAST",
    "T_LNUMBER T_DNUMBER - T_DNUMBER T_LNUMBER T_DNUMBER T_LNUMBER T_LNUMBER T_DNUMBER T_LNUMBER T_LNUMBER T_LNUMBER",
    "T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG T_VARIABLE T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG T_ELLIPSIS T_VARIABLE T_STRING T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG T_STRING T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG T_VARIABLE T_STRING T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG T_STRING T_STRING T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG T_VARIABLE T_STRING T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG T_VARIABLE T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG",
];

/// One row's tokens as `token_get_all` spells them: the open tag,
/// whitespace and comments left out, a single-character token itself, and
/// every other kind through the table that names it. The two rows the
/// tables do not cover, `T_VARIABLE` and `T_STRING` (`T_STRING_VARNAME`
/// after `${`), are what `ext/tokenizer` adds beside them.
fn names(row: &str) -> String {
    let input = format!("<?php {row}");
    let lexed = lex(F, input.as_bytes(), &LexOptions::default());
    let mut out: Vec<String> = Vec::new();
    let mut after_dollar_curly = false;
    for (kind, text) in pairs(&lexed, input.as_bytes()) {
        let end = (text.as_ptr() as usize - input.as_ptr() as usize) + text.len();
        let text = std::str::from_utf8(text).unwrap_or("<not ascii>");
        let name = match kind {
            OPEN_TAG | WHITESPACE | LINE_COMMENT | BLOCK_COMMENT => continue,
            VARIABLE => "T_VARIABLE",
            IDENT if after_dollar_curly => "T_STRING_VARNAME",
            IDENT => "T_STRING",
            MAGIC_CONST => magic_const_name(text.as_bytes()),
            CAST | CAST_REMOVED => cast_name(text.as_bytes()),
            INT if int_overflows(text.as_bytes()) => "T_DNUMBER",
            INT => "T_LNUMBER",
            AMP => amp_name(input.as_bytes(), end),
            _ => operator_name(kind).unwrap_or(text),
        };
        after_dollar_curly = kind == DOLLAR_L_CURLY;
        out.push(name.to_owned());
    }
    out.join(" ")
}

#[test]
fn the_tables_spell_what_token_get_all_spells() {
    for (row, pinned) in ROWS.iter().zip(PINNED) {
        assert_eq!(names(row), pinned, "row {row:?}");
    }
}

#[test]
fn int_overflows_reads_every_radix() {
    assert!(!int_overflows(b"9223372036854775807"));
    assert!(int_overflows(b"9223372036854775808"));
    assert!(!int_overflows(b"0x7FFF_FFFF_FFFF_FFFF"));
    assert!(int_overflows(b"0X8000000000000000"));
    assert!(!int_overflows(b"0777"));
    assert!(int_overflows(b"01000000000000000000000"));
    assert!(!int_overflows(b"0o777"));
    assert!(!int_overflows(b"0b1"));
    assert!(int_overflows(
        &[b'0', b'b', b'1']
            .into_iter()
            .chain([b'0'; 63])
            .collect::<Vec<_>>()
    ));
    assert!(!int_overflows(b"0"));
    // The prefix before a bad octal digit is the literal php reads.
    assert!(!int_overflows(b"08"));
    assert!(int_overflows(b"0_10000000000000000000009"));
}

#[test]
fn amp_name_looks_past_every_comment_shape() {
    assert_eq!(
        amp_name(b"& // c\n$x", 1),
        "T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG"
    );
    assert_eq!(
        amp_name(b"& # c\n...$x", 1),
        "T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG"
    );
    assert_eq!(
        amp_name(b"& #[A] $x", 1),
        "T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG"
    );
    assert_eq!(
        amp_name(b"& /* open", 1),
        "T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG"
    );
    assert_eq!(
        amp_name(b"&", 1),
        "T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG"
    );
}
