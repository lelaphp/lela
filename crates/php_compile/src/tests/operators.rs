//! Operators: arithmetic, shifts, concatenation, casts, comparisons and
//! increments, folded where php folds them and emitted as php's opcodes
//! where not.

use super::*;

#[test]
fn arithmetic_over_two_constants_is_folded_the_way_php_folds_it() {
    let (unit, _strings) = unit_and_strings(b"<?php echo 1 + 2;");
    // the sum is one literal and no ZEND_ADD
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO int(3)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(echoed_literal(b"<?php echo 1 + 2;").as_long(), Some(3));
    // `div_function`: a long where the division comes out whole, a
    // double where it does not.
    assert_eq!(echoed_literal(b"<?php echo 4/2;").as_long(), Some(2));
    assert_eq!(
        echoed_literal(b"<?php echo 1/3;").as_double(),
        Some(1.0 / 3.0)
    );
    assert_eq!(
        echoed_literal(b"<?php echo 0.1 + 0.2;").as_double(),
        Some(0.1 + 0.2)
    );
    // `fast_long_add_function`: the sum that overflows the long is
    // the double php leaves.
    assert_eq!(
        echoed_literal(b"<?php echo 9223372036854775807 + 1;").as_double(),
        Some(9_223_372_036_854_775_807_f64 + 1.0)
    );
    // `zend_try_ct_eval_unary_pm`: a multiplication by -1, which
    // keeps the sign of a zero.
    assert_eq!(echoed_literal(b"<?php echo -2;").as_long(), Some(-2));
    assert_eq!(
        echoed_literal(b"<?php echo -0.0;")
            .as_double()
            .map(f64::is_sign_negative),
        Some(true)
    );
}

/// `zend_try_ct_eval_binary_op` converts an operand as the opcode's
/// kernel converts it, through the one reader `php_heap` holds, so a
/// numeric string, a bool and null fold where they stand and the op
/// array is php's single `ECHO` of the folded constant.
#[test]
fn an_operand_php_converts_is_folded_where_it_stands() {
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO int(2)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php echo \"1\" + 1;"));
    assert_eq!(echoed_literal(b"<?php echo \"1\" + 1;").as_long(), Some(2));
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO int(2)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php echo true + 1;"));
    assert_eq!(echoed_literal(b"<?php echo true + 1;").as_long(), Some(2));
    // The folded 1 and the return's 1 are two literals, as
    // `zend_add_literal` appends each.
    let (unit, _strings) = unit_and_strings(b"<?php echo null + 1;");
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO int(1)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(unit.main.literals.len(), 2);
    assert_eq!(echoed_literal(b"<?php echo null + 1;").as_long(), Some(1));
    assert_eq!(echoed_literal(b"<?php echo false + 1;").as_long(), Some(1));
    // `_is_numeric_string_ex`: whitespace on either side, a sign, a
    // fraction, an exponent, and a long's edge.
    assert_eq!(
        echoed_literal(b"<?php echo \" 1 \" + \"+2\";").as_long(),
        Some(3)
    );
    assert_eq!(
        echoed_literal(b"<?php echo \"1e3\" + \".5\";").as_double(),
        Some(1000.5)
    );
    assert_eq!(
        echoed_literal(b"<?php echo \"9223372036854775808\" + 0;").as_double(),
        Some(9_223_372_036_854_775_808.0)
    );
    assert_eq!(
        echoed_literal(b"<?php echo -\"1.5\";").as_double(),
        Some(-1.5)
    );
    assert_eq!(echoed_literal(b"<?php echo -null;").as_long(), Some(0));
    assert_eq!(
        echoed_literal(b"<?php echo \"3\" ** \"2\";").as_long(),
        Some(9)
    );
}

#[test]
fn a_modulo_and_a_power_over_constants_fold_the_way_php_folds_them() {
    assert_eq!(echoed_literal(b"<?php echo 7 % 2;").as_long(), Some(1));
    assert_eq!(echoed_literal(b"<?php echo -7 % 2;").as_long(), Some(-1));
    assert_eq!(echoed_literal(b"<?php echo 7 % -2;").as_long(), Some(1));
    // `zend_is_op_long_compatible`: a double or a numeric string
    // that is a whole number in range casts to its long and folds.
    assert_eq!(echoed_literal(b"<?php echo 2.0 % 1;").as_long(), Some(0));
    assert_eq!(echoed_literal(b"<?php echo 7 % 2.0;").as_long(), Some(1));
    assert_eq!(
        echoed_literal(b"<?php echo \"1.0\" % 1;").as_long(),
        Some(0)
    );
    assert_eq!(
        echoed_literal(b"<?php echo \"1e3\" % 7;").as_long(),
        Some(6)
    );
    assert_eq!(echoed_literal(b"<?php echo 2 ** 3;").as_long(), Some(8));
    assert_eq!(
        echoed_literal(b"<?php echo 2 ** 3 ** 2;").as_long(),
        Some(512),
        "`**` reads right to left"
    );
    assert_eq!(
        echoed_literal(b"<?php echo -2 ** 2;").as_long(),
        Some(-4),
        "`**` binds tighter than unary `-`"
    );
    assert_eq!(
        echoed_literal(b"<?php echo 2 ** -1;").as_double(),
        Some(0.5)
    );
    // `pow_function_base`: the step that overflows leaves php's double.
    assert_eq!(
        echoed_literal(b"<?php echo 2 ** 63;").as_double(),
        Some(9_223_372_036_854_775_808_f64)
    );
}

/// `zend_try_ct_eval_binary_op` folds `+` over two constant arrays, the
/// one pair of arrays `zend_binary_op_produces_error` lets through, into
/// the array `add_function` makes, wherever the sum stands: assigned,
/// under `isset` and under `empty`, which php compiles as the `!` of it;
/// `-` over two arrays and `+` over an array and a number stay the
/// opcodes, which throw. The dump is the oracle's under `[regime.dump]` at
/// 8.5.9.
#[test]
fn two_constant_arrays_under_a_plus_fold_to_the_array_php_s_add_makes() {
    expect![[r#"

        $_main:
             ; (lines=12, args=0, vars=2, tmps=6)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($p) array(...)
        0001 ASSIGN CV1($q) array(...)
        0002 INIT_FCALL 4 144 string("var_dump")
        0003 T4 = ISSET_ISEMPTY_DIM_OBJ (isset) array(...) int(0)
        0004 SEND_VAL T4 1
        0005 SEND_VAL bool(true) 2
        0006 T5 = SUB array(...) array(...)
        0007 SEND_VAL T5 3
        0008 T6 = ADD array(...) int(1)
        0009 SEND_VAL T6 4
        0010 DO_ICALL
        0011 RETURN int(1)
    "#]].assert_eq(&dump_of(b"<?php $p = [0, 1] + [2]; $q = [] + []; var_dump(isset(([0, 1] + [])[0]), empty([] + []), [1] - [1], [1] + 1);"));
}

#[test]
#[allow(clippy::too_many_lines)] // one snapshot per case: the table is the test
fn arithmetic_php_does_not_fold_is_emitted_as_the_opcode_php_emits() {
    // php's own op array, its literals in the order the oplines name
    // them
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=0, tmps=2)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 1 96 string("extension_loaded")
        0001 SEND_VAL string("Core") 1
        0002 V0 = DO_ICALL
        0003 T1 = ADD int(1) V0
        0004 ECHO T1
        0005 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php echo 1 + extension_loaded('Core');"));
    // `zend_binary_op_produces_error`: a string that opens with no
    // number, or carries anything past the one it opens with, is left
    // to the opcode, which throws or warns where it stands.
    // The return's `1` is a slot of its own after the operands',
    // even beside an operand that is the same `1`, as
    // `zend_add_literal` appends it.
    for (source, expected) in [
        (
            &b"<?php echo \"abc\" + 1;"[..],
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = ADD string("abc") int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo \"1abc\" + 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = ADD string("1abc") int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo \"0x1A\" + 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = ADD string("0x1A") int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo \"1e\" + 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = ADD string("1e") int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 1 + \"\";",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = ADD int(1) string("")
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
    ] {
        let (unit, _strings) = unit_and_strings(source);
        expected.assert_eq(&unit.dump(&[]).text());
        assert_eq!(
            unit.main.oplines[2].op1,
            2,
            "{}",
            String::from_utf8_lossy(source)
        );
    }
    // The divisor php's `zval_get_double` reads as zero, whatever
    // its type, and the negative power of a zero.
    for (source, expected) in [
        (
            &b"<?php echo 2 / \"0\";"[..],
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = DIV int(2) string("0")
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 2 / \"0.0\";",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = DIV int(2) string("0.0")
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 2 / false;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = DIV int(2) bool(false)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 2 / null;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = DIV int(2) null
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
    ] {
        expected.assert_eq(&dump_of(source));
    }
    // `-"abc"` is the multiplication by -1 php emits for an operand
    // it will not fold.
    expect![[r#"

        $_main:
             ; (lines=3, args=0, vars=0, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T0 = MUL string("1abc") int(-1)
        0001 ECHO T0
        0002 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php echo -\"1abc\";"));
    // `zend_binary_op_produces_error`: the division php refuses to
    // fold, so that `DivisionByZeroError` is thrown where it stands.
    expect![[r#"

        $_main:
             ; (lines=3, args=0, vars=0, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T0 = DIV int(1) int(0)
        0001 ECHO T0
        0002 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php echo 1/0;"));
    // `zend_compile_unary_pm`: the multiplication by -1 php emits
    // where the operand is not a constant.
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=0, tmps=2)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 1 96 string("extension_loaded")
        0001 SEND_VAL string("Core") 1
        0002 V0 = DO_ICALL
        0003 T1 = MUL V0 int(-1)
        0004 ECHO T1
        0005 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php echo -extension_loaded('Core');"));
}

#[test]
#[allow(clippy::too_many_lines)] // one snapshot per case: the table is the test
fn a_modulo_and_a_power_php_does_not_fold_are_mod_and_pow_where_they_stand() {
    // The modulo php refuses to fold, by whatever `zval_get_long`
    // reads as zero and over an operand `zend_is_op_long_compatible`
    // refuses, and the negative power of a zero: each is the opcode,
    // to throw or deprecate where it stands.
    for (source, expected) in [
        (
            &b"<?php echo 1 % 0;"[..],
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = MOD int(1) int(0)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 1 % \"0\";",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = MOD int(1) string("0")
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 1 % null;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = MOD int(1) null
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 1.5 % 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = MOD float(1.5) int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo \"1.5\" % 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = MOD string("1.5") int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 1e19 % 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = MOD float(1e+19) int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo \"9223372036854775808\" % 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = MOD string("9223372036854775808") int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo INF % 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = MOD float(inf) int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
    ] {
        expected.assert_eq(&dump_of(source));
    }
    for (source, expected) in [
        (
            &b"<?php echo 0 ** -1;"[..],
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = POW int(0) int(-1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo \"0\" ** -1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = POW string("0") int(-1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 0.0 ** -1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = POW float(0) int(-1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
    ] {
        expected.assert_eq(&dump_of(source));
    }
    // `%` and `**` over a call, php's `MOD` and `POW` where `ADD`
    // stood above.
    for (source, expected) in [
        (
            &b"<?php echo 7 % extension_loaded('Core');"[..],
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 INIT_FCALL 1 96 string("extension_loaded")
                0001 SEND_VAL string("Core") 1
                0002 V0 = DO_ICALL
                0003 T1 = MOD int(7) V0
                0004 ECHO T1
                0005 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 2 ** extension_loaded('Core');",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 INIT_FCALL 1 96 string("extension_loaded")
                0001 SEND_VAL string("Core") 1
                0002 V0 = DO_ICALL
                0003 T1 = POW int(2) V0
                0004 ECHO T1
                0005 RETURN int(1)
            "#]],
        ),
    ] {
        expected.assert_eq(&dump_of(source));
    }
}

/// `shift_left_function` and `shift_right_function` over two
/// constants, as php folds them: the shift over the longs both sides
/// are worth, zero or the sign past the long's width, and the opcode
/// left standing for a negative count, an operand
/// `zend_is_op_long_compatible` refuses, or a call.
#[test]
fn a_shift_over_constants_folds_the_way_php_folds_it_and_the_rest_is_sl_or_sr() {
    assert_eq!(echoed_literal(b"<?php echo 1 << 3;").as_long(), Some(8));
    assert_eq!(echoed_literal(b"<?php echo 1 << 64;").as_long(), Some(0));
    assert_eq!(echoed_literal(b"<?php echo -1 >> 64;").as_long(), Some(-1));
    assert_eq!(echoed_literal(b"<?php echo 8 >> 1;").as_long(), Some(4));
    assert_eq!(echoed_literal(b"<?php echo \"2\" << 1;").as_long(), Some(4));
    assert_eq!(echoed_literal(b"<?php echo 1 << 2.0;").as_long(), Some(4));
    assert_eq!(
        echoed_literal(b"<?php echo 1 + 2 << 3;").as_long(),
        Some(24),
        "`+` binds tighter than `<<`"
    );
    for (source, expected) in [
        (
            &b"<?php echo 1 << -1;"[..],
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = SL int(1) int(-1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 1 >> -1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = SR int(1) int(-1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 1.5 << 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = SL float(1.5) int(1)
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo 1 << \"1x\";",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = SL int(1) string("1x")
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
    ] {
        expected.assert_eq(&dump_of(source));
    }
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=0, tmps=2)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 1 96 string("extension_loaded")
        0001 SEND_VAL string("Core") 1
        0002 V0 = DO_ICALL
        0003 T1 = SL int(1) V0
        0004 ECHO T1
        0005 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php echo 1 << extension_loaded('Core');"));
}

/// `zend_compile_binary_op`'s `ZEND_CONCAT` arm: the fold over two
/// constants through `concat_function`, each constant converted as
/// `_convert_to_string` converts it, so the op array is php's single
/// `ECHO` of the folded string; and a constant beside what does not
/// fold converted where it stands, so the `CONCAT` reads a string.
#[test]
#[allow(clippy::too_many_lines)] // one snapshot per case: the table is the test
fn a_concatenation_over_constants_folds_to_php_s_one_string_and_the_rest_is_concat() {
    for (source, expected) in [
        (
            &b"<?php echo 'a' . 'b';"[..],
            expect![[r#"

                $_main:
                     ; (lines=2, args=0, vars=0, tmps=0)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ECHO string("ab")
                0001 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo \"1\" . 2;",
            expect![[r#"

                $_main:
                     ; (lines=2, args=0, vars=0, tmps=0)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ECHO string("12")
                0001 RETURN int(1)
            "#]],
        ),
    ] {
        expected.assert_eq(&dump_of(source));
    }
    assert_eq!(echoed_string(b"<?php echo 'a' . 'b';"), b"ab");
    assert_eq!(echoed_string(b"<?php echo \"1\" . 2;"), b"12");
    assert_eq!(echoed_string(b"<?php echo 2.5 . 'x';"), b"2.5x");
    assert_eq!(echoed_string(b"<?php echo 1.0 . 'y';"), b"1y");
    assert_eq!(
        echoed_string(b"<?php echo 1 / 3 . '';"),
        b"0.33333333333333"
    );
    assert_eq!(echoed_string(b"<?php echo true . false . null;"), b"1");
    assert_eq!(echoed_string(b"<?php echo 'a' . 'b' . 'c';"), b"abc");
    assert_eq!(
        echoed_string(b"<?php echo 1 . 2 << 3;"),
        b"116",
        "`<<` binds tighter than `.`"
    );
    assert_eq!(
        echoed_literal(b"<?php echo (1 . 2) + 3;").as_long(),
        Some(15),
        "a folded `.` is a constant to the arithmetic around it"
    );
    // The precision the double is rendered at is the compile-time
    // ini entry, as `zend_double_to_str` reads `EG(precision)`.
    let mut strings = php_heap::Strings::new();
    let unit = compile_parsed(
        b"<?php echo 1 / 3 . '';",
        &Options {
            precision: 3,
            ..Options::default()
        },
        &mut strings,
    )
    .expect("the source compiles");
    assert_eq!(literal_text(&unit, 0), Some(b"0.333".to_vec()));
    // A constant beside a CV is converted and the `CONCAT` stands.
    let (unit, _strings) = unit_and_strings(
        b"<?php $a = 1; echo 2 . $a; echo $a . 2.5; echo 'a' . 'b' . $a; echo NAN . $a;",
    );
    // php's own op array
    expect![[r#"

        $_main:
             ; (lines=10, args=0, vars=1, tmps=5)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) int(1)
        0001 T2 = CONCAT string("2") CV0($a)
        0002 ECHO T2
        0003 T3 = CONCAT CV0($a) string("2.5")
        0004 ECHO T3
        0005 T4 = CONCAT string("ab") CV0($a)
        0006 ECHO T4
        0007 T5 = CONCAT string("NAN") CV0($a)
        0008 ECHO T5
        0009 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(
        literal_text(&unit, 1),
        Some(b"2".to_vec()),
        "the long is the string `2`"
    );
    assert_eq!(
        literal_text(&unit, 2),
        Some(b"2.5".to_vec()),
        "the double is the string `2.5`"
    );
    assert_eq!(
        literal_text(&unit, 3),
        Some(b"ab".to_vec()),
        "the left fold is one string"
    );
    assert_eq!(
        literal_text(&unit, 4),
        Some(b"NAN".to_vec()),
        "the NAN is the string `zend_double_to_str` renders"
    );
}

/// `zend_double_to_str` over a NAN raises the coercion warning while
/// the file compiles, and the unit carries it as opcache carries a
/// `persistent_script`'s `warnings`: `E_WARNING`, php's text, at the
/// line the fold stood on, once per operand converted and in the
/// order they were, with the folded string in the op array. A unit
/// with nothing raised carries nothing.
#[test]
fn a_nan_a_fold_converts_is_the_warning_the_unit_carries() {
    use php_unit::{Diagnostic, Level};
    let (unit, _strings) =
        unit_and_strings(b"<?php\necho 'a';\necho NAN . 'z';\necho 1 . NAN . NAN;");
    expect![[r#"

        $_main:
             ; (lines=4, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-4
             ; return  [] RANGE[0..0]
        0000 ECHO string("a")
        0001 ECHO string("NANz")
        0002 ECHO string("1NANNAN")
        0003 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    let coerced = |line| Diagnostic {
        level: Level::Warning,
        line,
        message: "unexpected NAN value was coerced to string".into(),
    };
    assert_eq!(
        unit.diagnostics,
        vec![coerced(3), coerced(4), coerced(4)],
        "one per operand converted, at the statement's line"
    );
    assert!(unit_of(b"<?php echo 'a' . 'b';").diagnostics.is_empty());
    assert!(
        unit_of(b"<?php echo NAN;").diagnostics.is_empty(),
        "an `ECHO` over a NAN converts nothing at compile time; the kernel warns"
    );
}

/// `zend_compile_cast` under the dump recipe: `CAST` with php's type
/// constant over the operand into a temporary, `BOOL` for `(bool)`,
/// nothing folded and the `FREE` after one nothing reads, which
/// `zend_do_free` leaves off a `BOOL`; and the scanner's escapes as
/// the bytes php reads.
#[test]
fn a_cast_is_php_s_opline_and_an_escape_is_php_s_byte() {
    expect![[r#"

        $_main:
             ; (lines=13, args=0, vars=2, tmps=7)
             ; (before optimizer)
             ; :1-8
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($x) int(1)
        0001 T3 = CAST (long) CV0($x)
        0002 FREE T3
        0003 T4 = CAST (double) int(1)
        0004 FREE T4
        0005 T5 = CAST (string) CV0($x)
        0006 ASSIGN CV1($s) T5
        0007 T7 = BOOL CV0($x)
        0008 T8 = CAST (long) string("12abc")
        0009 ECHO T8
        0010 ECHO string("\xFF")
        0011 ECHO string("NANz")
        0012 RETURN int(1)
    "#]].assert_eq(&dump_of(
            b"<?php\n$x = 1;\n(integer)$x;\n(float)1;\n$s = (string)$x;\n(boolean)$x;\necho (int)\"12abc\", \"\\777\", NAN . \"z\";\n",
        ));
}

/// `zend_compile_cast`'s other two arms: `(array)` and `(object)` are
/// `CAST` with `IS_ARRAY` and `IS_OBJECT` in `extended_value`, which
/// the dump prints as php's does.
#[test]
fn an_array_and_an_object_cast_are_php_s_oplines() {
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=3, tmps=5)
             ; (before optimizer)
             ; :1-5
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($x) int(1)
        0001 T4 = CAST (array) CV0($x)
        0002 ASSIGN CV1($a) T4
        0003 T6 = CAST (object) CV1($a)
        0004 ASSIGN CV2($o) T6
        0005 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php\n$x = 1;\n$a = (array)$x;\n$o = (object)$a;\n",
    ));
}

/// `zend_compile_unary_op` for `ZEND_BW_NOT`: one `BW_NOT`
/// where the operand is no constant, and
/// `zend_try_ct_eval_unary_op` where it is one php's
/// `zend_unary_op_produces_error` lets through. `~1` and `~1.0` are
/// `int(-2)` in the op array, `~"ab"` the bytes negated, and `~1.5`
/// the opline over `float(1.5)`, which the guard refuses to fold
/// because the long conversion loses something. `-~$a` is the
/// `MUL` by -1 `zend_compile_unary_pm` emits over the `BW_NOT`, and
/// `~$a instanceof C` is the `INSTANCEOF` over it, since `~` binds
/// tighter. The dump is the oracle's under `[regime.dump]` at
/// 8.5.9.
#[test]
fn a_bitwise_not_is_the_oracle_s_opline_or_its_fold() {
    expect![[r#"

        $_main:
             ; (lines=18, args=0, vars=9, tmps=17)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) int(6)
        0001 T10 = BW_NOT CV0($a)
        0002 ASSIGN CV1($b) T10
        0003 ASSIGN CV2($c) int(-2)
        0004 ASSIGN CV3($d) string("\x9E\x9D")
        0005 T14 = BW_NOT float(1.5)
        0006 ASSIGN CV4($e) T14
        0007 T16 = BW_NOT CV0($a)
        0008 T17 = BW_NOT T16
        0009 ASSIGN CV5($f) T17
        0010 T19 = BW_NOT CV0($a)
        0011 T20 = MUL T19 int(-1)
        0012 ASSIGN CV6($g) T20
        0013 T22 = BW_NOT CV0($a)
        0014 T23 = INSTANCEOF T22 string("C")
        0015 ASSIGN CV7($h) T23
        0016 ASSIGN CV8($i) int(-2)
        0017 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php $a = 6; $b = ~$a; $c = ~1; $d = ~\"ab\"; $e = ~1.5; $f = ~~$a; \
              $g = -~$a; $h = ~$a instanceof C; $i = ~1.0;",
    ));
}

/// `zend_eval_const_expr`'s `ZEND_AST_CAST` arm: the walk
/// inside an array expression folds every key and value in place
/// (`zend_try_ct_eval_array`'s first pass), so a cast over a
/// constant is the value it casts to even where the array itself is
/// no constant, and a cast over a variable stays the `CAST` opline.
/// `zend_compile_cast` folds nothing, so the same cast written
/// outside such a walk is the opline still.
#[test]
fn a_cast_inside_an_array_expression_folds_and_one_outside_it_does_not() {
    expect![[r#"
        0000 T3 = INIT_ARRAY 2 (packed) int(1) NEXT
        0001 T3 = ADD_ARRAY_ELEMENT CV1($b) NEXT
        0002 ASSIGN CV0($a) T3
        0003 T5 = INIT_ARRAY 2 (packed) string("1") NEXT
        0004 T6 = CAST (double) CV1($b)
        0005 T5 = ADD_ARRAY_ELEMENT T6 NEXT
        0006 ASSIGN CV2($c) T5
        0007 RETURN int(1)
        LIVE RANGES:
             3: 0001 - 0002 (tmp/var)
             5: 0004 - 0006 (tmp/var)
    "#]]
    .assert_eq(&oplines_of(
        b"<?php\n$a = [(int)\"1\", $b]; $c = [(string)1, (float)$b];\n",
    ));
    expect![[r#"
        0000 T1 = CAST (long) string("1")
        0001 ASSIGN CV0($x) T1
        0002 T3 = CAST (long) string("2")
        0003 ECHO T3
        0004 RETURN int(1)
    "#]]
    .assert_eq(&oplines_of(b"<?php\n$x = (int)\"1\";\necho (int)\"2\";\n"));
    // `zend_try_ct_eval_cast`: `(bool)`, `(int)`, `(float)`,
    // `(string)` and `(array)` fold, a `(string)` of a double or an
    // array does not, since one reads `precision` and the other
    // warns, an `(int)` of a double past the long's range does not,
    // no cast over a NAN does, and there is no `(object)` arm.
    let (unit, mut strings) = unit_and_strings(
        b"<?php\nclass C { const B = (bool)\"a\"; const I = (int)\"12abc\"; const F = (float)\"1e3\"; const S = (string)true; const A = (array)1; }\n",
    );
    let array = strings
        .constant_array(&[(php_heap::Key::Long(0), php_heap::Zval::long(1))], 1)
        .map(php_heap::Zval::array)
        .ok()
        .and_then(php_unit::decl::UnitValue::new)
        .map_or(ConstExpr::Null, ConstExpr::UnitValue);
    assert_eq!(
        unit.classes[0]
            .constants
            .iter()
            .map(|c| c.value.clone())
            .collect::<Vec<_>>(),
        vec![
            ConstExpr::Bool(true),
            ConstExpr::long(12),
            ConstExpr::Double(1000.0),
            ConstExpr::Str(b"1".as_slice().into()),
            array,
        ]
    );
}

/// The oracle's dump under the pin's `dump` regime, `vars=1, tmps=4`:
/// a `<` over a CV is `IS_SMALLER` into a temporary, `$a++` and
/// `++$a` whose values are read are `POST_INC` and `PRE_INC`, and a
/// `<` over two constants folds to the bool `zend_compare` orders
/// them to, a string pair and a double against a string included.
#[test]
fn a_comparison_is_is_smaller_where_it_does_not_fold_and_the_bool_where_it_does() {
    expect![[r#"

        $_main:
             ; (lines=16, args=0, vars=1, tmps=4)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T1 = IS_SMALLER CV0($a) int(2)
        0001 ECHO T1
        0002 T2 = IS_SMALLER int(1) CV0($a)
        0003 ECHO T2
        0004 T3 = POST_INC CV0($a)
        0005 ECHO T3
        0006 T4 = PRE_INC CV0($a)
        0007 ECHO T4
        0008 ECHO bool(true)
        0009 ECHO bool(true)
        0010 ECHO bool(true)
        0011 ECHO bool(false)
        0012 ECHO bool(false)
        0013 ECHO bool(true)
        0014 ECHO bool(true)
        0015 RETURN int(1)
    "#]].assert_eq(&dump_of(
            b"<?php echo $a < 2, 1 < $a, $a++, ++$a, 1 < 2, 1.5 < 2, \"a\" < \"b\", 2 < 1, \"10\" < \"9\", 1.5 < \"abc\", null < true;"
        ));
}

/// The oracle's dump, `vars=1, tmps=19`: `<=` is `IS_SMALLER_OR_EQUAL`
/// as written, `>` and `>=` are `IS_SMALLER` and `IS_SMALLER_OR_EQUAL`
/// with the operands swapped (`zend_compile_greater`), and the
/// equality level is its own opcode each, `<>` being `IS_NOT_EQUAL`.
/// `2 == $a` dumps as `IS_EQUAL CV0($a) int(2)`, and so do `2 * $a`
/// and the other three equalities, since `pass_two` swaps the
/// operands of a `COMMUTATIVE` opcode whose first is the lesser
/// kind; `2 <=> $a` and `2 + $a` keep their order.
#[test]
fn the_rest_of_the_comparison_level_is_the_opcode_php_emits_with_php_s_operand_order() {
    expect![[r#"

        $_main:
             ; (lines=39, args=0, vars=1, tmps=19)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T1 = IS_SMALLER_OR_EQUAL CV0($a) int(2)
        0001 ECHO T1
        0002 T2 = IS_SMALLER_OR_EQUAL int(2) CV0($a)
        0003 ECHO T2
        0004 T3 = IS_SMALLER int(2) CV0($a)
        0005 ECHO T3
        0006 T4 = IS_SMALLER CV0($a) int(2)
        0007 ECHO T4
        0008 T5 = IS_SMALLER_OR_EQUAL int(2) CV0($a)
        0009 ECHO T5
        0010 T6 = IS_SMALLER_OR_EQUAL CV0($a) int(2)
        0011 ECHO T6
        0012 T7 = IS_EQUAL CV0($a) int(2)
        0013 ECHO T7
        0014 T8 = IS_EQUAL CV0($a) int(2)
        0015 ECHO T8
        0016 T9 = IS_NOT_EQUAL CV0($a) int(2)
        0017 ECHO T9
        0018 T10 = IS_NOT_EQUAL CV0($a) int(2)
        0019 ECHO T10
        0020 T11 = IS_NOT_EQUAL CV0($a) int(2)
        0021 ECHO T11
        0022 T12 = IS_IDENTICAL CV0($a) int(2)
        0023 ECHO T12
        0024 T13 = IS_IDENTICAL CV0($a) int(2)
        0025 ECHO T13
        0026 T14 = IS_NOT_IDENTICAL CV0($a) int(2)
        0027 ECHO T14
        0028 T15 = IS_NOT_IDENTICAL CV0($a) int(2)
        0029 ECHO T15
        0030 T16 = SPACESHIP CV0($a) int(2)
        0031 ECHO T16
        0032 T17 = SPACESHIP int(2) CV0($a)
        0033 ECHO T17
        0034 T18 = MUL CV0($a) int(2)
        0035 ECHO T18
        0036 T19 = ADD int(2) CV0($a)
        0037 ECHO T19
        0038 RETURN int(1)
    "#]].assert_eq(&dump_of(
            b"<?php echo $a <= 2, 2 <= $a, $a > 2, 2 > $a, $a >= 2, 2 >= $a, $a == 2, 2 == $a, $a != 2, 2 != $a, $a <> 2, $a === 2, 2 === $a, $a !== 2, 2 !== $a, $a <=> 2, 2 <=> $a, 2 * $a, 2 + $a;"
        ));
}

/// The oracle's dump, `vars=0, tmps=0`: every comparison over two
/// constants folds, to the bool `zend_compare` orders them to, the
/// long `compare_function` writes for `<=>`, and `zend_is_identical`
/// for `===` and `!==`, which reads the type before the value, so
/// `1 === "1"` and `null === false` are false where `==` says true.
#[test]
fn every_comparison_over_two_constants_folds_to_the_value_php_folds_it_to() {
    expect![[r#"

        $_main:
             ; (lines=23, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO bool(true)
        0001 ECHO bool(false)
        0002 ECHO bool(false)
        0003 ECHO bool(true)
        0004 ECHO bool(true)
        0005 ECHO bool(true)
        0006 ECHO bool(false)
        0007 ECHO bool(false)
        0008 ECHO bool(true)
        0009 ECHO bool(false)
        0010 ECHO bool(true)
        0011 ECHO int(-1)
        0012 ECHO int(1)
        0013 ECHO int(0)
        0014 ECHO int(1)
        0015 ECHO int(-1)
        0016 ECHO bool(true)
        0017 ECHO bool(false)
        0018 ECHO bool(true)
        0019 ECHO bool(true)
        0020 ECHO bool(true)
        0021 ECHO bool(false)
        0022 RETURN int(1)
    "#]].assert_eq(&dump_of(
            b"<?php echo 1 <= 2, 2 <= 1, 1 > 2, 2 > 1, 1 >= 1, 1 == \"1\", 1 != \"1\", \"abc\" == \"ABC\", 1 === 1, 1 === \"1\", 1 !== 1.0, 1 <=> 2, 2 <=> 1, 1 <=> 1, \"10\" <=> \"9\", 1.5 <=> \"abc\", null == false, null === false, \"1e3\" == \"1000\", \"abc\" <= \"abd\", 1.5 >= \"1.5\", 2 > \"10\";"
        ));
}

/// The oracle's dump, `vars=2, tmps=7`: a `===` or `!==` against a
/// constant null, false or true on either side is the `TYPE_CHECK`
/// `zend_compile_binary_op` emits in its place, the constant's
/// `1 << IS_*` in `extended_value` for `===` and `MAY_BE_ANY` less
/// that bit for `!==`; against any other constant, or a variable,
/// it is the opcode itself, and a `==` against null is `IS_EQUAL`.
#[test]
fn an_identity_against_null_or_a_bool_is_the_type_check_php_emits_in_its_place() {
    expect![[r#"

        $_main:
             ; (lines=15, args=0, vars=2, tmps=7)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T2 = TYPE_CHECK (null) CV0($a)
        0001 ECHO T2
        0002 T3 = TYPE_CHECK (null) CV0($a)
        0003 ECHO T3
        0004 T4 = TYPE_CHECK TYPE [null, true, long, double, string, array, object, resource] CV0($a)
        0005 ECHO T4
        0006 T5 = TYPE_CHECK TYPE [null, false, long, double, string, array, object, resource] CV0($a)
        0007 ECHO T5
        0008 T6 = IS_IDENTICAL CV0($a) int(1)
        0009 ECHO T6
        0010 T7 = IS_EQUAL CV0($a) null
        0011 ECHO T7
        0012 T8 = IS_NOT_IDENTICAL CV0($a) CV1($b)
        0013 ECHO T8
        0014 RETURN int(1)
    "#]].assert_eq(&dump_of(
            b"<?php echo $a === null, null === $a, $a !== false, true !== $a, $a === 1, $a == null, $a !== $b;"
        ));
}

/// `zend_do_free` over every place php frees a value: an expression
/// statement, each expression of a `for`'s three lists but the last
/// of the condition, and the last of the initialisers and the step.
/// Each `$a++` there is php's `PRE_INC`; the one an `ASSIGN` reads
/// stays `POST_INC`. The oracle's dump is the same thirteen oplines,
/// `vars=2, tmps=10`.
#[test]
fn a_post_inc_nothing_reads_is_the_pre_inc_zend_do_free_makes_it() {
    expect![[r#"

        $_main:
             ; (lines=13, args=0, vars=2, tmps=10)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 PRE_INC CV0($a)
        0001 PRE_INC CV0($a)
        0002 T4 = POST_INC CV0($a)
        0003 ASSIGN CV1($b) T4
        0004 PRE_INC CV0($a)
        0005 PRE_INC CV1($b)
        0006 JMP 0009
        0007 PRE_INC CV0($a)
        0008 PRE_INC CV0($a)
        0009 PRE_INC CV0($a)
        0010 T11 = IS_SMALLER CV0($a) int(1)
        0011 JMPNZ T11 0007
        0012 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php $a++; ++$a; $b = $a++; for ($a++, $b++; $a++, $a < 1; $a++, ++$a) {}",
    ));
}

/// The same over `--`: `$a--;` as a statement is php's `PRE_DEC
/// CV0($a)`, `zend_do_free`'s conversion of the `POST_DEC` nothing
/// reads, and the ones an `ASSIGN`, an `ECHO` or a comparison reads
/// stay `POST_DEC`. The oracle's dump is the same fifteen oplines,
/// `vars=2, tmps=10`.
#[test]
fn a_post_dec_nothing_reads_is_the_pre_dec_zend_do_free_makes_it() {
    expect![[r#"

        $_main:
             ; (lines=15, args=0, vars=2, tmps=10)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 PRE_DEC CV0($a)
        0001 PRE_DEC CV0($a)
        0002 T4 = POST_DEC CV0($a)
        0003 ASSIGN CV1($b) T4
        0004 T6 = PRE_DEC CV0($a)
        0005 ECHO T6
        0006 T7 = POST_DEC CV0($a)
        0007 ECHO T7
        0008 PRE_DEC CV0($a)
        0009 JMP 0011
        0010 PRE_DEC CV0($a)
        0011 T10 = POST_DEC CV0($a)
        0012 T11 = IS_SMALLER int(1) T10
        0013 JMPNZ T11 0010
        0014 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php $a--; --$a; $b = $a--; echo --$a, $a--; for ($a--; $a-- > 1; --$a) {}",
    ));
}
