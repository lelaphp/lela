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
pub fn compile_source(source: &[u8], options: &Options) -> Result<Unit, CompileError> {
    compile(&php_syntax::parse(source), options)
}

/// Compile one parsed file into a unit.
pub fn compile(parsed: &Parsed, options: &Options) -> Result<Unit, CompileError> {
    let read = read::read(&parsed.source).map_err(|trouble| CompileError {
        message: trouble.what,
        line: trouble.line,
    })?;
    let lines = u32::try_from(parsed.source.split(|b| *b == b'\n').count()).unwrap_or(u32::MAX);
    Ok(Unit {
        main: emit::main_op_array(&read, &options.filename, lines),
        ..Unit::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use php_unit::{Opcode, OperandKind, OplineIdx};

    fn unit_of(source: &[u8]) -> Unit {
        compile(&php_syntax::parse(source), &Options::default()).unwrap_or_default()
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
            "$_main:\n     ; (lines=7, args=0, vars=0, tmps=1)\n\
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

    #[test]
    fn what_the_reader_cannot_read_is_a_compile_error_at_its_line() {
        let trouble = compile(&php_syntax::parse(b"<?php\n$x = 1;"), &Options::default()).err();
        assert_eq!(trouble.map(|e| e.line), Some(2));
    }
}
