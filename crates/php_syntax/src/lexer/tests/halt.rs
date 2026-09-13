//! `__halt_compiler();` and the verbatim data trailer, binary included:
//! Composer's and phar's stubs.

use super::*;

#[test]
fn halt_compiler_swallows_trailing_data() {
    check(
        "<?php __halt_compiler();DATA ?> not php",
        &[
            (OPEN_TAG, "<?php "),
            (HALT_COMPILER_KW, "__halt_compiler"),
            (L_PAREN, "("),
            (R_PAREN, ")"),
            (SEMICOLON, ";"),
            (HALT_DATA, "DATA ?> not php"),
        ],
    );
}

#[test]
fn halt_compiler_case_insensitive() {
    check(
        "<?php __HALT_COMPILER();x",
        &[
            (OPEN_TAG, "<?php "),
            (HALT_COMPILER_KW, "__HALT_COMPILER"),
            (L_PAREN, "("),
            (R_PAREN, ")"),
            (SEMICOLON, ";"),
            (HALT_DATA, "x"),
        ],
    );
}

#[test]
fn halt_compiler_with_trivia_between_pieces() {
    check(
        "<?php __halt_compiler /*c*/ ( ) ;d",
        &[
            (OPEN_TAG, "<?php "),
            (HALT_COMPILER_KW, "__halt_compiler"),
            (WHITESPACE, " "),
            (BLOCK_COMMENT, "/*c*/"),
            (WHITESPACE, " "),
            (L_PAREN, "("),
            (WHITESPACE, " "),
            (R_PAREN, ")"),
            (WHITESPACE, " "),
            (SEMICOLON, ";"),
            (HALT_DATA, "d"),
        ],
    );
}

#[test]
fn halt_compiler_empty_trailer_emits_nothing() {
    check(
        "<?php __halt_compiler();",
        &[
            (OPEN_TAG, "<?php "),
            (HALT_COMPILER_KW, "__halt_compiler"),
            (L_PAREN, "("),
            (R_PAREN, ")"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn halt_data_may_be_binary() {
    check_bytes(
        b"<?php __halt_compiler();\x00\xFF\x01",
        &[
            (OPEN_TAG, b"<?php "),
            (HALT_COMPILER_KW, b"__halt_compiler"),
            (L_PAREN, b"("),
            (R_PAREN, b")"),
            (SEMICOLON, b";"),
            (HALT_DATA, b"\x00\xFF\x01"),
        ],
    );
    // No diagnostic for the binary trailer.
    assert_eq!(diags(b"<?php __halt_compiler();\x00\xFF\x01"), vec![]);
}

#[test]
fn deviation_disarms_the_recognizer() {
    // `__halt_compiler(1);` is not the halt statement shape; lexing
    // continues as ordinary PHP after the `;`.
    check(
        "<?php __halt_compiler(1);echo 2;",
        &[
            (OPEN_TAG, "<?php "),
            (HALT_COMPILER_KW, "__halt_compiler"),
            (L_PAREN, "("),
            (INT, "1"),
            (R_PAREN, ")"),
            (SEMICOLON, ";"),
            (ECHO_KW, "echo"),
            (WHITESPACE, " "),
            (INT, "2"),
            (SEMICOLON, ";"),
        ],
    );
}

#[test]
fn halt_as_property_name_does_not_arm() {
    // ST_LOOKING_FOR_PROPERTY remaps the label to IDENT, so the recognizer
    // never sees HALT_COMPILER_KW.
    check(
        "<?php $x->__halt_compiler();1;",
        &[
            (OPEN_TAG, "<?php "),
            (VARIABLE, "$x"),
            (ARROW, "->"),
            (IDENT, "__halt_compiler"),
            (L_PAREN, "("),
            (R_PAREN, ")"),
            (SEMICOLON, ";"),
            (INT, "1"),
            (SEMICOLON, ";"),
        ],
    );
}
