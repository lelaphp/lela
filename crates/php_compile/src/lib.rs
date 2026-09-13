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

use php_heap::Intern;
use php_syntax::Parsed;
use php_unit::Unit;
use php_unit::decl::Name;

/// What the compiler is told beside the source (design §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    /// The file name the unit and its errors carry.
    pub filename: Name,
    /// Whether the caller's `declare(strict_types=1)` is in force.
    pub strict_types: bool,
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
        main: emit::main_op_array(&read, &options.filename, lines, strings)?,
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
        let unit = unit_of(b"<?php echo 1;");
        assert_eq!(
            unit.dump(),
            "$_main:\n     ; (lines=2, args=0, vars=0, tmps=0)\n\
             0000 ECHO CONST[0]\n0001 RETURN CONST[0]\n",
            "the constant 1 is echoed and returned, and php keeps one slot for it"
        );
    }

    #[test]
    fn a_nested_call_sends_what_the_inner_call_returned() {
        let unit = unit_of(b"<?php var_dump(extension_loaded('demo'));");
        assert_eq!(
            unit.dump(),
            "$_main:\n     ; (lines=7, args=0, vars=0, tmps=2)\n\
             0000 INIT_FCALL CONST[0]\n\
             0001 INIT_FCALL CONST[1]\n\
             0002 SEND_VAL CONST[2]\n\
             0003 DO_ICALL\n\
             0004 SEND_VAR V0\n\
             0005 DO_ICALL\n\
             0006 RETURN CONST[3]\n"
        );
        assert_eq!(unit.main.oplines[0].extended_value, 1, "one argument");
        assert_eq!(unit.main.cache_size, 2, "one cache slot per INIT_FCALL");
        assert_eq!(
            unit.main.num_tmps, 2,
            "one temporary per call, as php counts them here"
        );
    }

    #[test]
    fn the_entry_is_safepoint_zero_and_the_return_carries_its_own_number() {
        let unit = unit_of(b"<?php echo 1;");
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
        let unit = unit_of(source);
        unit.main
            .literals
            .first()
            .copied()
            .unwrap_or(php_heap::Zval::UNDEF)
    }

    #[test]
    fn arithmetic_over_two_constants_is_folded_the_way_php_folds_it() {
        let unit = unit_of(b"<?php echo 1 + 2;");
        assert_eq!(
            unit.dump(),
            "$_main:\n     ; (lines=2, args=0, vars=0, tmps=0)\n\
             0000 ECHO CONST[0]\n0001 RETURN CONST[1]\n",
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
    fn what_php_leaves_to_an_opcode_is_refused_rather_than_folded() {
        for source in [
            // `zend_binary_op_produces_error`: the division that throws.
            b"<?php echo 1/0;".as_slice(),
            b"<?php echo 1/0.0;".as_slice(),
            // A constant no table here holds.
            b"<?php echo FOO;".as_slice(),
            // An operand php converts and this does not.
            b"<?php echo \"1\" + 1;".as_slice(),
            // What a call returned is known to no compiler.
            b"<?php echo 1 + extension_loaded('demo');".as_slice(),
        ] {
            let mut strings = php_heap::Strings::new();
            assert!(
                compile(
                    &php_syntax::parse(source),
                    &Options::default(),
                    &mut strings
                )
                .is_err(),
                "{}",
                String::from_utf8_lossy(source)
            );
        }
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

    #[test]
    fn what_the_reader_cannot_read_is_a_compile_error_at_its_line() {
        let mut strings = php_heap::Strings::new();
        let trouble = compile(
            &php_syntax::parse(b"<?php\n$x = 1;"),
            &Options::default(),
            &mut strings,
        )
        .err();
        assert_eq!(trouble.map(|e| e.line), Some(2));
    }
}
