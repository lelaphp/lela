//! The compiler (design §4.1, §6.1): one pass from the typed tree to a
//! [`Unit`], after `zend_compile.c`, with the constant-expression tree,
//! early binding, compile-time fatals at the site that owns each
//! construct, and safepoint numbering with its invariant asserted. What is
//! here is the first slice of Lane B (design §11): the statements the
//! engine's loop serves, `echo` and a call of a named function, emitted as
//! php emits them, with the entry safepoint and the numbered return. The
//! tree it walks is `read`'s while `php_syntax` is a stub (design §4.9);
//! what it cannot read is refused as a [`CompileError`].
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod emit;
mod read;
mod special;

/// The token stream (design §4.9), handed up for `php_engine::api::syntax`
/// and nothing of the compiler's own: the lexer, its stream and php's
/// names for its kinds, which `token_get_all` spells. It passes through
/// here for the same reason [`compile_source`] does: the engine's edges
/// reach this crate and not the syntax crate (design §4.2).
pub mod syntax {
    pub use php_syntax::{
        FileId, LexOptions, Lexed, SyntaxKind, Token, amp_name, cast_name, int_overflows, lex,
        magic_const_name, operator_name,
    };
}

use php_heap::Intern;
use php_syntax::Parsed;
use php_unit::Unit;
use php_unit::decl::{ConstDecl, Name};

/// What the compiler is told beside the source (design §4.1).
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// The file name the unit and its errors carry.
    pub filename: Name,
    /// Whether the caller's `declare(strict_types=1)` is in force.
    pub strict_types: bool,
    /// php's `precision` ini entry as it stands while the file compiles,
    /// which `_convert_to_string` renders a double constant at when the
    /// compiler folds a `.` over it (`zend_double_to_str`, which reads
    /// `EG(precision)`); php's default is 14.
    pub precision: i32,
    /// The persistent constants the engine's extensions declared
    /// (`CONST_PERSISTENT`), which `zend_try_ct_eval_const` substitutes
    /// where they are written; a name not here is fetched as the program serves.
    pub persistent: Vec<ConstDecl>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            filename: Name::default(),
            strict_types: false,
            precision: 14,
            persistent: Vec::new(),
        }
    }
}

/// A compile-time fatal: php's message, at php's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    pub message: String,
    pub line: u32,
}

/// Parse and compile one source file: what the engine calls for the
/// file it serves, an `include` or an `eval`, since its edges reach the compiler
/// and not the syntax crate (design §4.2).
///
/// `strings` is where the unit's constant strings are interned (design
/// §5.2): the engine's table for a unit bound for the unit cache, whose
/// strings every request serving it then shares, and the request's own
/// heap for a unit that is one request's alone. php decides the same
/// thing by which handler is installed when `zend_compile_file` runs
/// (`Z/Zend/zend_string.c:363-373`).
pub fn compile_source(
    source: &[u8],
    options: &Options,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    compile(&php_syntax::parse(source), options, strings)
}

/// Compile one parsed file into a unit, interning its constant strings
/// into `strings`.
pub fn compile(
    parsed: &Parsed,
    options: &Options,
    strings: &mut dyn Intern,
) -> Result<Unit, CompileError> {
    let read = read::read(&parsed.source).map_err(|trouble| CompileError {
        message: trouble.what,
        line: trouble.line,
    })?;
    let lines = u32::try_from(parsed.source.split(|b| *b == b'\n').count()).unwrap_or(u32::MAX);
    Ok(Unit {
        main: emit::main_op_array(&read, options, lines, strings)?,
        ..Unit::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_unit::{Opcode, OperandKind, OplineIdx};

    /// A unit whose constant strings are one engine's, which is what the
    /// unit cache holds (design §6.3).
    fn unit_of(source: &[u8]) -> Unit {
        unit_and_strings(source).0
    }

    /// php's dump of a one-line file's main op array, the file unnamed
    /// as `Options::default` leaves it: the header with these counts,
    /// then the oplines.
    fn main_dump(lines: usize, vars: u32, tmps: u32, oplines: &str) -> String {
        format!(
            "\n$_main:\n     ; (lines={lines}, args=0, vars={vars}, tmps={tmps})\n\
             \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n{oplines}"
        )
    }

    /// The dump of one source, read while the table its strings were
    /// interned into still stands.
    fn dump_of(source: &[u8]) -> String {
        let (unit, _strings) = unit_and_strings(source);
        unit.dump()
    }

    fn unit_and_strings(source: &[u8]) -> (Unit, php_heap::Strings) {
        let mut strings = php_heap::Strings::new();
        let unit = compile(
            &php_syntax::parse(source),
            &Options::default(),
            &mut strings,
        )
        .unwrap_or_default();
        (unit, strings)
    }

    #[test]
    fn an_empty_file_compiles_the_way_php_compiles_one() {
        let unit = unit_of(b"");
        assert_eq!(unit.main.oplines.len(), 1);
        assert_eq!(unit.main.oplines[0].opcode, Opcode::Return);
        assert_eq!(unit.main.oplines[0].op1_type, OperandKind::Const);
        assert_eq!(
            unit.main.oplines[0].extended_value,
            u32::MAX,
            "php sets the implicit final return's extended_value to -1"
        );
        assert!(unit.functions.is_empty());
    }

    #[test]
    fn echo_of_a_number_is_one_echo_and_the_final_return() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1;");
        assert_eq!(
            unit.dump(),
            main_dump(2, 0, 0, "0000 ECHO int(1)\n0001 RETURN int(1)\n"),
            "the constant 1 is echoed and returned"
        );
        assert_eq!(
            (unit.main.oplines[0].op1, unit.main.oplines[1].op1),
            (0, 0),
            "php keeps one slot for it"
        );
    }

    #[test]
    fn a_nested_call_sends_what_the_inner_call_returned() {
        let (unit, _strings) = unit_and_strings(b"<?php var_dump(extension_loaded('demo'));");
        assert_eq!(
            unit.dump(),
            main_dump(
                7,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"var_dump\")\n\
                 0001 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0002 SEND_VAL string(\"demo\") 1\n\
                 0003 DO_ICALL\n\
                 0004 SEND_VAR V0 1\n\
                 0005 DO_ICALL\n\
                 0006 RETURN int(1)\n"
            ),
            "php's, but for the `V0 = ` php opens the inner call with (design §5.7)"
        );
        assert_eq!(unit.main.oplines[0].extended_value, 1, "one argument");
        assert_eq!(
            unit.main.oplines[0].op1, 96,
            "zend_vm_calc_used_stack: five header slots and one argument, sixteen bytes each"
        );
        assert_eq!(unit.main.oplines[1].op1, 96);
        assert_eq!(unit.main.cache_size, 2, "one cache slot per INIT_FCALL");
        assert_eq!(
            unit.main.num_tmps, 2,
            "one temporary per call, as php counts them here"
        );
    }

    #[test]
    fn the_entry_is_safepoint_zero_and_the_return_carries_its_own_number() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1;");
        assert_eq!(
            unit.main.safepoints.as_ref(),
            &[OplineIdx(0), OplineIdx(1)],
            "the entry at opline zero, then the return"
        );
        assert_eq!(
            unit.main.oplines[1].out, 1,
            "a site opcode carries its safepoint number in the field ops.toml names"
        );
    }

    /// The one constant a single-expression `echo` leaves in the table.
    fn echoed_literal(source: &[u8]) -> php_heap::Zval {
        let (unit, _strings) = unit_and_strings(source);
        unit.main
            .literals
            .first()
            .copied()
            .unwrap_or(php_heap::Zval::UNDEF)
    }

    #[test]
    fn arithmetic_over_two_constants_is_folded_the_way_php_folds_it() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1 + 2;");
        assert_eq!(
            unit.dump(),
            main_dump(2, 0, 0, "0000 ECHO int(3)\n0001 RETURN int(1)\n"),
            "the sum is one literal and no ZEND_ADD"
        );
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
        assert_eq!(
            dump_of(b"<?php echo \"1\" + 1;"),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n")
        );
        assert_eq!(echoed_literal(b"<?php echo \"1\" + 1;").as_long(), Some(2));
        assert_eq!(
            dump_of(b"<?php echo true + 1;"),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n")
        );
        assert_eq!(echoed_literal(b"<?php echo true + 1;").as_long(), Some(2));
        // The folded 1 and the return's 1 are one literal, as
        // `zend_add_literal` keeps them.
        let (unit, _strings) = unit_and_strings(b"<?php echo null + 1;");
        assert_eq!(
            unit.dump(),
            main_dump(2, 0, 0, "0000 ECHO int(1)\n0001 RETURN int(1)\n")
        );
        assert_eq!(unit.main.literals.len(), 1);
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
    fn a_persistent_constant_stands_for_its_value_where_it_is_written() {
        assert_eq!(
            echoed_literal(b"<?php echo INF;").as_double(),
            Some(f64::INFINITY)
        );
        assert_eq!(
            echoed_literal(b"<?php echo -INF;").as_double(),
            Some(f64::NEG_INFINITY)
        );
        assert_eq!(
            echoed_literal(b"<?php echo \\NAN;")
                .as_double()
                .map(f64::is_nan),
            Some(true),
            "a leading separator names the same global constant"
        );
    }

    #[test]
    fn the_engine_s_persistent_constants_stand_for_their_values_where_they_are_written() {
        use php_unit::decl::ConstExpr;
        let mut strings = php_heap::Strings::new();
        let options = Options {
            persistent: vec![
                ConstDecl {
                    name: Name(b"T_LNUMBER".to_vec().into_boxed_slice()),
                    value: ConstExpr::Long(260),
                },
                ConstDecl {
                    name: Name(b"PHP_OS".to_vec().into_boxed_slice()),
                    value: ConstExpr::Str(b"Darwin".to_vec().into_boxed_slice()),
                },
            ],
            ..Options::default()
        };
        let compiled = compile(
            &php_syntax::parse(b"<?php echo T_LNUMBER - 1, \\PHP_OS;"),
            &options,
            &mut strings,
        );
        assert!(compiled.is_ok(), "{compiled:?}");
        let unit = compiled.unwrap_or_default();
        assert_eq!(
            unit.dump(),
            "$_main:\n     ; (lines=3, args=0, vars=0, tmps=0)\n\
             0000 ECHO CONST[0]\n\
             0001 ECHO CONST[1]\n\
             0002 RETURN CONST[2]\n"
        );
        assert_eq!(unit.main.literals[0].as_long(), Some(259));
        assert_eq!(
            unit.main.literals[1].as_str().map(|s| s.bytes().to_vec()),
            Some(b"Darwin".to_vec())
        );
        assert!(
            compile(
                &php_syntax::parse(b"<?php echo t_lnumber;"),
                &options,
                &mut strings
            )
            .is_err(),
            "a constant's name is case-sensitive"
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

    #[test]
    fn arithmetic_php_does_not_fold_is_emitted_as_the_opcode_php_emits() {
        assert_eq!(
            dump_of(b"<?php echo 1 + extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 DO_ICALL\n\
                 0003 ADD int(1) V0\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            ),
            "php's own op array, its literals in the order the oplines name them"
        );
        // `zend_binary_op_produces_error`: a string that opens with no
        // number, or carries anything past the one it opens with, is left
        // to the opcode, which throws or warns where it stands.
        // The return's `1` shares the slot of an operand that is the
        // same `1`, as `zend_add_literal` keeps them.
        for (source, left, right, returned) in [
            (
                &b"<?php echo \"abc\" + 1;"[..],
                "string(\"abc\")",
                "int(1)",
                1,
            ),
            (b"<?php echo \"1abc\" + 1;", "string(\"1abc\")", "int(1)", 1),
            (b"<?php echo \"0x1A\" + 1;", "string(\"0x1A\")", "int(1)", 1),
            (b"<?php echo \"1e\" + 1;", "string(\"1e\")", "int(1)", 1),
            (b"<?php echo 1 + \"\";", "int(1)", "string(\"\")", 0),
        ] {
            let (unit, _strings) = unit_and_strings(source);
            assert_eq!(
                unit.dump(),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 ADD {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
            assert_eq!(
                unit.main.oplines[2].op1,
                returned,
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // The divisor php's `zval_get_double` reads as zero, whatever
        // its type, and the negative power of a zero.
        for (source, divisor) in [
            (&b"<?php echo 2 / \"0\";"[..], "string(\"0\")"),
            (b"<?php echo 2 / \"0.0\";", "string(\"0.0\")"),
            (b"<?php echo 2 / false;", "bool(false)"),
            (b"<?php echo 2 / null;", "null"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 DIV int(2) {divisor}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // `-"abc"` is the multiplication by -1 php emits for an operand
        // it will not fold.
        assert_eq!(
            dump_of(b"<?php echo -\"1abc\";"),
            main_dump(
                3,
                0,
                1,
                "0000 MUL string(\"1abc\") int(-1)\n0001 ECHO T0\n0002 RETURN int(1)\n"
            )
        );
        // `zend_binary_op_produces_error`: the division php refuses to
        // fold, so that `DivisionByZeroError` is thrown where it stands.
        assert_eq!(
            dump_of(b"<?php echo 1/0;"),
            main_dump(
                3,
                0,
                1,
                "0000 DIV int(1) int(0)\n0001 ECHO T0\n0002 RETURN int(1)\n"
            )
        );
        // `zend_compile_unary_pm`: the multiplication by -1 php emits
        // where the operand is not a constant.
        assert_eq!(
            dump_of(b"<?php echo -extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 DO_ICALL\n\
                 0003 MUL V0 int(-1)\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            )
        );
    }

    #[test]
    fn a_modulo_and_a_power_php_does_not_fold_are_mod_and_pow_where_they_stand() {
        // The modulo php refuses to fold, by whatever `zval_get_long`
        // reads as zero and over an operand `zend_is_op_long_compatible`
        // refuses, and the negative power of a zero: each is the opcode,
        // to throw or deprecate where it stands.
        for (source, left, right) in [
            (&b"<?php echo 1 % 0;"[..], "int(1)", "int(0)"),
            (b"<?php echo 1 % \"0\";", "int(1)", "string(\"0\")"),
            (b"<?php echo 1 % null;", "int(1)", "null"),
            (b"<?php echo 1.5 % 1;", "float(1.5)", "int(1)"),
            (b"<?php echo \"1.5\" % 1;", "string(\"1.5\")", "int(1)"),
            (b"<?php echo 1e19 % 1;", "float(1e+19)", "int(1)"),
            (
                b"<?php echo \"9223372036854775808\" % 1;",
                "string(\"9223372036854775808\")",
                "int(1)",
            ),
            (b"<?php echo INF % 1;", "float(inf)", "int(1)"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 MOD {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for (source, base) in [
            (&b"<?php echo 0 ** -1;"[..], "int(0)"),
            (b"<?php echo \"0\" ** -1;", "string(\"0\")"),
            (b"<?php echo 0.0 ** -1;", "float(0)"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 POW {base} int(-1)\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        // `%` and `**` over a call, php's `MOD` and `POW` where `ADD`
        // stood above.
        for (source, opcode, left) in [
            (
                &b"<?php echo 7 % extension_loaded('Core');"[..],
                "MOD",
                "int(7)",
            ),
            (
                b"<?php echo 2 ** extension_loaded('Core');",
                "POW",
                "int(2)",
            ),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    6,
                    0,
                    2,
                    &format!(
                        "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                         0001 SEND_VAL string(\"Core\") 1\n\
                         0002 DO_ICALL\n\
                         0003 {opcode} {left} V0\n\
                         0004 ECHO T1\n\
                         0005 RETURN int(1)\n"
                    )
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
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
        for (source, opcode, left, right) in [
            (&b"<?php echo 1 << -1;"[..], "SL", "int(1)", "int(-1)"),
            (b"<?php echo 1 >> -1;", "SR", "int(1)", "int(-1)"),
            (b"<?php echo 1.5 << 1;", "SL", "float(1.5)", "int(1)"),
            (b"<?php echo 1 << \"1x\";", "SL", "int(1)", "string(\"1x\")"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    3,
                    0,
                    1,
                    &format!("0000 {opcode} {left} {right}\n0001 ECHO T0\n0002 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        assert_eq!(
            dump_of(b"<?php echo 1 << extension_loaded('Core');"),
            main_dump(
                6,
                0,
                2,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"Core\") 1\n\
                 0002 DO_ICALL\n\
                 0003 SL int(1) V0\n\
                 0004 ECHO T1\n\
                 0005 RETURN int(1)\n"
            )
        );
    }

    /// The text of the one constant a single-expression `echo` leaves,
    /// read while the table it was interned into still stands.
    fn echoed_string(source: &[u8]) -> Vec<u8> {
        let (unit, _strings) = unit_and_strings(source);
        literal_text(&unit, 0).unwrap_or_default()
    }

    /// The text of the constant at `at`, for a string; nothing otherwise.
    fn literal_text(unit: &Unit, at: usize) -> Option<Vec<u8>> {
        unit.main
            .literals
            .get(at)
            .and_then(|z| z.as_str())
            .map(|s| s.bytes().to_vec())
    }

    /// `zend_compile_binary_op`'s `ZEND_CONCAT` arm: the fold over two
    /// constants through `concat_function`, each constant converted as
    /// `_convert_to_string` converts it, so the op array is php's single
    /// `ECHO` of the folded string; and a constant beside what does not
    /// fold converted where it stands, so the `CONCAT` reads a string.
    #[test]
    fn a_concatenation_over_constants_folds_to_php_s_one_string_and_the_rest_is_concat() {
        for (source, joined) in [
            (&b"<?php echo 'a' . 'b';"[..], "ab"),
            (b"<?php echo \"1\" . 2;", "12"),
        ] {
            assert_eq!(
                dump_of(source),
                main_dump(
                    2,
                    0,
                    0,
                    &format!("0000 ECHO string(\"{joined}\")\n0001 RETURN int(1)\n")
                ),
                "{}",
                String::from_utf8_lossy(source)
            );
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
        let unit = compile(
            &php_syntax::parse(b"<?php echo 1 / 3 . '';"),
            &Options {
                precision: 3,
                ..Options::default()
            },
            &mut strings,
        )
        .unwrap_or_default();
        assert_eq!(literal_text(&unit, 0), Some(b"0.333".to_vec()));
        // A constant beside a CV is converted and the `CONCAT` stands.
        let (unit, _strings) = unit_and_strings(
            b"<?php $a = 1; echo 2 . $a; echo $a . 2.5; echo 'a' . 'b' . $a; echo NAN . $a;",
        );
        assert_eq!(
            unit.dump(),
            main_dump(
                10,
                1,
                5,
                "0000 ASSIGN CV0($a) int(1)\n\
                 0001 CONCAT string(\"2\") CV0($a)\n\
                 0002 ECHO T2\n\
                 0003 CONCAT CV0($a) string(\"2.5\")\n\
                 0004 ECHO T3\n\
                 0005 CONCAT string(\"ab\") CV0($a)\n\
                 0006 ECHO T4\n\
                 0007 CONCAT float(nan) CV0($a)\n\
                 0008 ECHO T5\n\
                 0009 RETURN int(1)\n"
            ),
            "php's, but for the `string(\"NAN\")` php converts the NAN to (#218)"
        );
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
            unit.main
                .literals
                .get(4)
                .and_then(|z| z.as_double())
                .map(f64::is_nan),
            Some(true),
            "a NAN is left as written until the compiler can warn (#218)"
        );
    }

    /// `zend_compile_for` over `for (;;) {}`, php's own oplines under
    /// the dump recipe: the `JMP` forward to the condition, and the
    /// condition, the constant `true` for an empty one, as the `JMPNZ`
    /// back to the body's first opline, which is itself here. php folds
    /// nothing, so the back-edge is a `JMPNZ` and not a `JMP`.
    #[test]
    fn an_empty_for_is_php_s_forward_jump_and_a_conditional_jump_to_itself() {
        let (unit, _strings) = unit_and_strings(b"<?php set_time_limit(1); for (;;) {}");
        assert_eq!(
            unit.dump(),
            main_dump(
                6,
                0,
                1,
                "0000 INIT_FCALL 1 96 string(\"set_time_limit\")\n\
                 0001 SEND_VAL int(1) 1\n\
                 0002 DO_ICALL\n\
                 0003 JMP 0004\n\
                 0004 JMPNZ bool(true) 0004\n\
                 0005 RETURN int(1)\n"
            ),
            "the oracle's dump, byte for byte"
        );
        assert_eq!(
            unit.main.oplines[3].op1, 4,
            "the JMP lands on the condition"
        );
        assert_eq!(
            unit.main.oplines[4].op2, 4,
            "the JMPNZ lands on the body's first opline, which is itself"
        );
        assert_eq!(
            unit.main.literals[2],
            php_heap::Zval::bool(true),
            "an empty condition is php's constant true"
        );
        assert_eq!(
            unit.main.safepoints.as_ref(),
            &[OplineIdx(0), OplineIdx(4), OplineIdx(5)],
            "the entry, the back-edge, the return"
        );
        assert_eq!(
            unit.main.oplines[4].out, 1,
            "the back-edge's safepoint number"
        );
        assert_eq!(unit.main.oplines[5].out, 2, "the return's");
    }

    /// The three expression lists and the body, in the order php lays
    /// them down: initialisers, the jump, the body, the step, then the
    /// condition, whose last expression is what the `JMPNZ` tests. The
    /// oracle's dump of this source, under the recipe, is the same
    /// sixteen oplines with `V3 = DO_ICALL` at 13, the one call whose
    /// value is read.
    #[test]
    fn a_for_lays_its_lists_down_in_php_s_order() {
        let (unit, _strings) = unit_and_strings(
            b"<?php for (extension_loaded('i'); extension_loaded('a'), extension_loaded('b'); extension_loaded('s')) echo 'body';",
        );
        assert_eq!(
            unit.dump(),
            main_dump(
                16,
                0,
                4,
                "0000 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0001 SEND_VAL string(\"i\") 1\n\
                 0002 DO_ICALL\n\
                 0003 JMP 0008\n\
                 0004 ECHO string(\"body\")\n\
                 0005 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0006 SEND_VAL string(\"s\") 1\n\
                 0007 DO_ICALL\n\
                 0008 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0009 SEND_VAL string(\"a\") 1\n\
                 0010 DO_ICALL\n\
                 0011 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
                 0012 SEND_VAL string(\"b\") 1\n\
                 0013 DO_ICALL\n\
                 0014 JMPNZ V3 0004\n\
                 0015 RETURN int(1)\n"
            )
        );
        assert_eq!(
            unit.main.oplines[3].op1, 8,
            "the JMP lands on the condition"
        );
        assert_eq!(unit.main.oplines[14].op2, 4, "the JMPNZ lands on the body");
    }

    /// `zend_compile_while`, and a block whose statements keep their own
    /// lines while the loop's own oplines keep the statement's, as php's
    /// `CG(zend_lineno)` is the condition's again once the condition is
    /// compiled; the final return takes the line the last opline left,
    /// as `zend_emit_final_return` does.
    #[test]
    fn a_while_is_the_same_shape_and_the_body_keeps_its_lines() {
        let (unit, _strings) =
            unit_and_strings(b"<?php\nwhile (extension_loaded('a')) {\n  echo 1;\n}");
        assert_eq!(
            unit.dump(),
            "\n$_main:\n     ; (lines=7, args=0, vars=0, tmps=1)\n\
             \x20    ; (before optimizer)\n     ; :1-4\n     ; return  [] RANGE[0..0]\n\
             0000 JMP 0002\n\
             0001 ECHO int(1)\n\
             0002 INIT_FCALL 1 96 string(\"extension_loaded\")\n\
             0003 SEND_VAL string(\"a\") 1\n\
             0004 DO_ICALL\n\
             0005 JMPNZ V0 0001\n\
             0006 RETURN int(1)\n"
        );
        assert_eq!(unit.main.oplines[0].op1, 2);
        assert_eq!(unit.main.oplines[5].op2, 1);
        let lines: Vec<u32> = unit.main.oplines.iter().map(|op| op.lineno).collect();
        assert_eq!(lines, vec![2, 3, 2, 2, 2, 2, 2]);
    }

    #[test]
    fn what_this_compiler_cannot_reach_is_refused_rather_than_mis_compiled() {
        // A constant no table here holds.
        let mut strings = php_heap::Strings::new();
        assert!(
            compile(
                &php_syntax::parse(b"<?php echo FOO;"),
                &Options::default(),
                &mut strings
            )
            .is_err()
        );
    }

    /// The oracle's dump under the pin's `dump` regime of
    /// `$a = 1; echo $a, -$a, $A; $c = $b = $a + 1;`, `vars=4, tmps=5`:
    ///
    /// ```text
    /// 0000 ASSIGN CV0($a) int(1)
    /// 0001 ECHO CV0($a)
    /// 0002 T5 = MUL CV0($a) int(-1)
    /// 0003 ECHO T5
    /// 0004 ECHO CV1($A)
    /// 0005 T6 = ADD CV0($a) int(1)
    /// 0006 T7 = ASSIGN CV3($b) T6
    /// 0007 ASSIGN CV2($c) T7
    /// 0008 RETURN int(1)
    /// ```
    ///
    /// `lookup_cv` numbers a name when it is first met, `$a` and `$A`
    /// apart, and `zend_compile_assign` looks the variable up before it
    /// compiles the value, so `$c` is met before `$b`. Each `ASSIGN`
    /// takes a temporary, read or not, which is the five php counts.
    #[test]
    fn a_variable_is_its_cv_and_an_assignment_is_php_s_assign_in_php_s_order() {
        use OperandKind::{Const, Cv, Tmp, Unused};
        let unit = unit_of(b"<?php $a = 1; echo $a, -$a, $A; $c = $b = $a + 1;");
        assert_eq!(
            unit.main.vars.iter().map(|v| &*v.0).collect::<Vec<_>>(),
            [b"a".as_slice(), b"A", b"c", b"b"]
        );
        assert_eq!(unit.main.num_cvs, 4);
        assert_eq!(unit.main.num_tmps, 5);
        let shape: Vec<_> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.op1_type, op.op1, op.op2_type, op.op2, op.out))
            .collect();
        assert_eq!(
            shape,
            [
                (Opcode::Assign, Cv, 0, Const, 0, 0),
                (Opcode::Echo, Cv, 0, Unused, 0, 0),
                (Opcode::Mul, Cv, 0, Const, 1, 1),
                (Opcode::Echo, Tmp, 1, Unused, 0, 0),
                (Opcode::Echo, Cv, 1, Unused, 0, 0),
                (Opcode::Add, Cv, 0, Const, 0, 2),
                (Opcode::Assign, Cv, 3, Tmp, 2, 3),
                (Opcode::Assign, Cv, 2, Tmp, 3, 4),
                (Opcode::Return, Const, 0, Unused, 0, 1),
            ]
        );
    }

    /// A CV is sent by `SEND_VAR` as a call's `VAR` is
    /// (`zend_compile_args`), and the value a native returned is
    /// assigned from its `VAR`, which is the shape #174's witness
    /// compiles to.
    #[test]
    fn a_cv_is_sent_by_send_var_and_assigned_from_a_call_s_var() {
        use OperandKind::{Const, Cv, Unused, Var};
        let unit = unit_of(b"<?php $b = memory_get_usage(); var_dump(memory_get_usage() - $b);");
        let shape: Vec<_> = unit
            .main
            .oplines
            .iter()
            .map(|op| (op.opcode, op.op1_type, op.op2_type))
            .collect();
        assert_eq!(
            shape,
            [
                (Opcode::InitFcall, Unused, Const),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Assign, Cv, Var),
                (Opcode::InitFcall, Unused, Const),
                (Opcode::InitFcall, Unused, Const),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Sub, Var, Cv),
                (Opcode::SendVal, OperandKind::Tmp, Unused),
                (Opcode::DoIcall, Unused, Unused),
                (Opcode::Return, Const, Unused),
            ]
        );
        let sent = unit_of(b"<?php $a = 1; var_dump($a);");
        assert_eq!(sent.main.oplines[2].opcode, Opcode::SendVar);
        assert_eq!(sent.main.oplines[2].op1_type, Cv);
    }

    /// Two units compiled into one engine's table share the one string
    /// for a constant both carry, and a constant carried twice in one
    /// unit is interned once: that is what interning is for, and it is
    /// what a published unit hands every request serving it (design
    /// §5.2, §6.3).
    #[test]
    fn two_units_interned_into_one_table_share_their_constant_string() {
        let mut strings = php_heap::Strings::new();
        let compiled = [
            compile_source(b"<?php echo 'shared';", &Options::default(), &mut strings),
            compile_source(
                b"<?php echo 'shared', 'shared';",
                &Options::default(),
                &mut strings,
            ),
        ];
        assert_eq!(strings.count(), 1, "one string, carried by two units");
        let named: Vec<Option<php_heap::Str>> = compiled
            .iter()
            .map(|unit| {
                unit.as_ref()
                    .ok()
                    .and_then(|unit| unit.main.literals.first().copied())
                    .and_then(php_heap::Zval::as_str)
            })
            .collect();
        assert!(named.iter().all(Option::is_some), "{named:?}");
        assert_eq!(
            named[0].map(|s| s.bytes().to_vec()),
            Some(b"shared".to_vec())
        );
        let [Some(one), Some(other)] = [named[0], named[1]] else {
            return;
        };
        assert!(one.is_same(other), "one `zend_string`, named by both units");
    }

    /// The oracle's dump under the pin's `dump` regime of #162's third
    /// program, `echo 1 + strlen('a');`:
    ///
    /// ```text
    /// 0000 ECHO int(2)
    /// 0001 RETURN int(1)
    /// ```
    ///
    /// `zend_try_compile_special_func` reaches `zend_compile_func_strlen`,
    /// which leaves the constant string's length where the call stood,
    /// and `zend_try_ct_eval_binary_op` folds the sum over it.
    #[test]
    fn strlen_over_a_constant_string_is_its_length_and_the_sum_folds_over_it() {
        let (unit, _strings) = unit_and_strings(b"<?php echo 1 + strlen('a');");
        assert_eq!(
            unit.dump(),
            main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n"),
            "one ECHO of a constant, no INIT_FCALL and no ADD"
        );
        assert_eq!(unit.main.literals[0].as_long(), Some(2));
    }

    /// The constant a single-expression `echo` leaves, where the whole
    /// expression folded: the first slot, of two with the return's.
    fn folded(source: &[u8]) -> Option<php_heap::Zval> {
        let (unit, _strings) = unit_and_strings(source);
        (unit.main.oplines.len() == 2 && unit.main.oplines[0].opcode == Opcode::Echo)
            .then(|| unit.main.literals.first().copied())
            .flatten()
    }

    /// Whether the source compiles to the ordinary call of `name`: an
    /// `INIT_FCALL` first, over that lowered name, and a `DO_ICALL`.
    fn ordinary_call(source: &[u8], name: &[u8]) -> bool {
        let (unit, _strings) = unit_and_strings(source);
        let oplines = &unit.main.oplines;
        oplines.first().map(|op| op.opcode) == Some(Opcode::InitFcall)
            && literal_text(&unit, 0).as_deref() == Some(name)
            && oplines.iter().any(|op| op.opcode == Opcode::DoIcall)
    }

    /// The four rows of php's table that fold to a constant with no
    /// opcode, each held to what the oracle dumps: `strlen` over a
    /// constant string, `chr` over a long written 0 to 255, `ord` over a
    /// one-byte string, `defined` over a name the compiler substitutes,
    /// and `.` over two literals counting as written, since php's parser
    /// folds it before the compiler looks (`zend_ast_create_concat_op`).
    #[test]
    fn the_four_special_names_fold_to_the_constant_php_dumps() {
        for (source, long) in [
            (&b"<?php echo strlen('a' . 'b');"[..], 2),
            (b"<?php echo strlen('a' . 1);", 2),
            (b"<?php echo strlen('');", 0),
            (b"<?php echo \\strlen('ab');", 2),
            (b"<?php echo STRLEN('ab');", 2),
            (b"<?php echo strlen(chr(65));", 1),
            (b"<?php echo ord('a');", 97),
            (b"<?php echo ord('a' . '');", 97),
        ] {
            assert_eq!(
                folded(source).and_then(php_heap::Zval::as_long),
                Some(long),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for (source, text) in [
            (&b"<?php echo chr(65);"[..], &b"A"[..]),
            (b"<?php echo chr(0);", b"\0"),
            (b"<?php echo chr(255);", b"\xff"),
        ] {
            assert_eq!(
                echoed_string(source),
                text,
                "{}",
                String::from_utf8_lossy(source)
            );
        }
        for source in [
            &b"<?php echo defined('INF');"[..],
            b"<?php echo defined('NAN');",
            b"<?php echo defined('true');",
            b"<?php echo defined('TRUE');",
            b"<?php echo defined('null');",
            b"<?php echo defined('IN' . 'F');",
        ] {
            assert_eq!(
                folded(source).map(php_heap::Zval::kind),
                Some(php_heap::ZvalType::True),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// What php leaves to an opcode (`ZEND_STRLEN`, `ZEND_DEFINED`,
    /// `ZEND_COUNT`) or to the call itself is the ordinary call here,
    /// and never a constant it is not: a `strlen` over anything but a
    /// constant string, a `chr` whose long is not written as such or is
    /// outside the byte, an `ord` over any other string, a `defined`
    /// over a namespaced name, an unknown one or no string, a wrong
    /// count of arguments, and a name whose row is an opcode.
    #[test]
    fn a_special_name_this_compiler_cannot_fold_is_the_ordinary_call() {
        for (source, name) in [
            (&b"<?php echo strlen(1);"[..], &b"strlen"[..]),
            (b"<?php echo strlen(INF);", b"strlen"),
            (b"<?php echo strlen(null);", b"strlen"),
            (b"<?php echo strlen($a);", b"strlen"),
            (b"<?php echo strlen();", b"strlen"),
            (b"<?php echo strlen('a', 'b');", b"strlen"),
            (b"<?php echo chr(1 + 64);", b"chr"),
            (b"<?php echo chr(-0);", b"chr"),
            (b"<?php echo chr(-1);", b"chr"),
            (b"<?php echo chr(256);", b"chr"),
            (b"<?php echo chr('65');", b"chr"),
            (b"<?php echo ord('ab');", b"ord"),
            (b"<?php echo ord('');", b"ord"),
            (b"<?php echo ord(65);", b"ord"),
            (b"<?php echo defined('A\\\\B');", b"defined"),
            (b"<?php echo defined('A::B');", b"defined"),
            (b"<?php echo defined('NOPE');", b"defined"),
            (b"<?php echo defined('inf');", b"defined"),
            (b"<?php echo defined(1.5);", b"defined"),
            (b"<?php echo defined(null);", b"defined"),
            (b"<?php echo count('x');", b"count"),
            (b"<?php echo is_int(1);", b"is_int"),
        ] {
            assert!(
                ordinary_call(source, name),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
    }

    /// The table is `zend_try_compile_special_func_ex`'s names in its
    /// order, `strlen` first and `clone` last, and a name is found by the
    /// row it stands in.
    #[test]
    fn the_special_table_is_php_s_names_in_php_s_order() {
        use special::{Special, TABLE, row};
        assert_eq!(TABLE.len(), 35);
        assert_eq!(TABLE.first().map(|(n, _)| *n), Some(&b"strlen"[..]));
        assert_eq!(TABLE.last().map(|(n, _)| *n), Some(&b"clone"[..]));
        assert_eq!(row(b"defined"), Some(Special::Defined));
        assert_eq!(row(b"is_integer"), Some(Special::Opcode));
        assert_eq!(row(b"sizeof"), Some(Special::Opcode));
        assert_eq!(row(b"var_dump"), None);
        let names: Vec<&[u8]> = TABLE.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "each name once");
    }

    #[test]
    fn what_the_reader_cannot_read_is_a_compile_error_at_its_line() {
        let mut strings = php_heap::Strings::new();
        let trouble = compile(
            &php_syntax::parse(b"<?php\n$x == 1;"),
            &Options::default(),
            &mut strings,
        )
        .err();
        assert_eq!(trouble.map(|e| e.line), Some(2));
    }
}
