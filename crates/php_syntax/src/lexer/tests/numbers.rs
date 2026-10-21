//! Numeric literals (Appendix C: numeric underscores, `0o`, legacy octal
//! with invalid digits, hex/binary edge widths, exponent floats).

use super::*;

#[test]
fn decimal_ints() {
    check_php("0", &[(INT, "0")]);
    check_php("123", &[(INT, "123")]);
    check_php("1_000_000", &[(INT, "1_000_000")]);
}

#[test]
fn hex_binary_octal() {
    check_php("0xFF", &[(INT, "0xFF")]);
    check_php("0Xab_cd", &[(INT, "0Xab_cd")]);
    check_php("0b1010", &[(INT, "0b1010")]);
    check_php("0B1_0", &[(INT, "0B1_0")]);
    check_php("0o17", &[(INT, "0o17")]);
    check_php("0O7_7", &[(INT, "0O7_7")]);
    check_php("0777", &[(INT, "0777")]);
    check_php("0_7", &[(INT, "0_7")]);
}

#[test]
fn hex_stops_at_non_hex_digit() {
    check_php("0xFFg", &[(INT, "0xFF"), (IDENT, "g")]);
    check_php("0b12", &[(INT, "0b1"), (INT, "2")]);
}

#[test]
fn floats() {
    check_php("1.5", &[(FLOAT, "1.5")]);
    check_php(".5", &[(FLOAT, ".5")]);
    check_php("1.", &[(FLOAT, "1.")]);
    check_php("1.e3", &[(FLOAT, "1.e3")]);
    check_php("1e10", &[(FLOAT, "1e10")]);
    check_php("1E+5", &[(FLOAT, "1E+5")]);
    check_php(".5e-2", &[(FLOAT, ".5e-2")]);
    check_php("1_0.5_0e1_0", &[(FLOAT, "1_0.5_0e1_0")]);
    // Leading zero plus a float part is legal (octal-ness applies to ints).
    check_php("08.5", &[(FLOAT, "08.5")]);
    check_php("09e2", &[(FLOAT, "09e2")]);
}

#[test]
fn exponent_needs_digits() {
    // EXPONENT_DNUM fails without digits; LNUM then a label win.
    check_php("1e", &[(INT, "1"), (IDENT, "e")]);
    check_php("1e+", &[(INT, "1"), (IDENT, "e"), (PLUS, "+")]);
    check_php(
        "1e+_2",
        &[(INT, "1"), (IDENT, "e"), (PLUS, "+"), (IDENT, "_2")],
    );
}

#[test]
fn adjacent_dots() {
    // Flex longest match: DNUM("1.") then DNUM(".2").
    check_php("1..2", &[(FLOAT, "1."), (FLOAT, ".2")]);
    check_php("1.5.5", &[(FLOAT, "1.5"), (FLOAT, ".5")]);
    check_php("..5", &[(DOT, "."), (FLOAT, ".5")]);
}

#[test]
fn malformed_numbers_cut_where_zends_pattern_ends() {
    // What `token_get_all` cuts under the pin: the number is the longest
    // match and the rest is the identifier php's parser then names.
    check_php("0x", &[(INT, "0"), (IDENT, "x")]);
    check_php("0b", &[(INT, "0"), (IDENT, "b")]);
    check_php("0o", &[(INT, "0"), (IDENT, "o")]);
    check_php("0xZ", &[(INT, "0"), (IDENT, "xZ")]);
    check_php("0o8", &[(INT, "0"), (IDENT, "o8")]);
    check_php("08", &[(INT, "08")]);
    check_php("0_8", &[(INT, "0_8")]);
    check_php("1_", &[(INT, "1"), (IDENT, "_")]);
    check_php("1__2", &[(INT, "1"), (IDENT, "__2")]);
    check_php("1_.5", &[(INT, "1"), (IDENT, "_"), (FLOAT, ".5")]);
    check_php("1._5", &[(FLOAT, "1."), (IDENT, "_5")]);
    check_php("1_e5", &[(INT, "1"), (IDENT, "_e5")]);
    check_php("0x_FF", &[(INT, "0"), (IDENT, "x_FF")]);
    check_php("0x1_", &[(INT, "0x1"), (IDENT, "_")]);
    check_php("1_2_", &[(INT, "1_2"), (IDENT, "_")]);
}
