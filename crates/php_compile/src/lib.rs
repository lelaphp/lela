//! The compiler (design §4.1, §6.1): one pass from the typed tree to a
//! [`Unit`], after `zend_compile.c`, with the constant-expression tree,
//! early binding, compile-time fatals at the site that owns each
//! construct, and safepoint numbering with its invariant asserted. This
//! skeleton emits what `compile-identity` reads as all red: a main op
//! array of one `RETURN` of the constant `null`, php's shape for an empty
//! file, and nothing for anything else. The compiler is Lane B of design §11.
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

use php_heap::Zval;
use php_syntax::Parsed;
use php_unit::decl::Name;
use php_unit::{OpArray, Opcode, OperandKind, Opline, OplineIdx, Unit};

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
///
/// The stub compiles every input as php compiles an empty file: one
/// `RETURN` of the constant `null` in the main op array, with the entry
/// safepoint at opline zero, and no functions, classes or constants.
pub fn compile(parsed: &Parsed, options: &Options) -> Result<Unit, CompileError> {
    let _ = parsed;
    let mut main = OpArray {
        filename: options.filename.clone(),
        line_start: 1,
        line_end: 1,
        ..OpArray::default()
    };
    main.literals = vec![Zval::null()].into_boxed_slice();
    main.oplines = vec![Opline {
        opcode: Opcode::Return,
        op1_type: OperandKind::Const,
        op1: 0,
        lineno: 1,
        ..Opline::default()
    }]
    .into_boxed_slice();
    main.safepoints = vec![OplineIdx(0)].into_boxed_slice();
    Ok(Unit {
        main,
        ..Unit::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_compiles_an_empty_file_the_way_php_does() {
        let parsed = php_syntax::parse(b"");
        let unit = compile(&parsed, &Options::default()).unwrap_or_default();
        assert_eq!(unit.main.oplines.len(), 1);
        assert_eq!(unit.main.oplines[0].opcode, Opcode::Return);
        assert_eq!(unit.main.safepoints.as_ref(), &[OplineIdx(0)]);
        assert!(unit.functions.is_empty());
    }
}
