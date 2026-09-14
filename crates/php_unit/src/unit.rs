//! A unit (design §4.7): one compiled file, immutable once published, its
//! declarations beside their op arrays.

use crate::decl::{ClassDecl, ConstDecl, FunctionDecl};
use crate::op::OpArray;

/// A compiled PHP program: the main op array, the functions with their
/// bodies at the same index, the classes and the constants declared,
/// and the diagnostics compiling it raised.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Unit {
    pub main: OpArray,
    pub functions: Vec<(FunctionDecl, OpArray)>,
    pub classes: Vec<ClassDecl>,
    pub constants: Vec<ConstDecl>,
    /// What `zend_error` raised while the file compiled, in the order
    /// it raised them: a coercion warning from a fold, a deprecation on
    /// a declaration. Kept beside the unit as opcache keeps a
    /// `persistent_script`'s `warnings`
    /// (`Z/ext/opcache/ZendAccelerator.h:130`), so the engine raises
    /// each before the main op array runs, on every
    /// request the unit serves, as php raises them once per compilation
    /// and opcache replays them on a hit (`zend_emit_recorded_errors_ex`,
    /// `Z/ext/opcache/ZendAccelerator.c:1939`).
    pub diagnostics: Vec<Diagnostic>,
}

/// php's error levels, `E_*`: what a diagnostic is raised at, and what
/// `error_reporting` masks by the bit [`Level::bit`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    Error,
    Warning,
    Parse,
    Notice,
    CoreError,
    CoreWarning,
    CompileError,
    CompileWarning,
    UserError,
    UserWarning,
    UserNotice,
    Strict,
    RecoverableError,
    Deprecated,
    UserDeprecated,
}

impl Level {
    /// php's number for the level, what `error_reporting` masks.
    #[must_use]
    pub const fn bit(self) -> i64 {
        match self {
            Level::Error => 1,
            Level::Warning => 2,
            Level::Parse => 4,
            Level::Notice => 8,
            Level::CoreError => 16,
            Level::CoreWarning => 32,
            Level::CompileError => 64,
            Level::CompileWarning => 128,
            Level::UserError => 256,
            Level::UserWarning => 512,
            Level::UserNotice => 1024,
            Level::Strict => 2048,
            Level::RecoverableError => 4096,
            Level::Deprecated => 8192,
            Level::UserDeprecated => 16384,
        }
    }
}

/// One diagnostic the compiler raised, php's `zend_error_info`
/// (`Z/Zend/zend.h:129`) less the file, which is the unit's own: the
/// level, the line `zend_get_compiled_lineno` answered when it was
/// raised, and php's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub line: u32,
    pub message: Box<str>,
}

impl Unit {
    /// The op arrays in php's dump format, what `zend_dump_op_array`
    /// prints under `opcache.opt_debug_level=0x10000` and what
    /// `compile-identity` compares with the oracle's (design §6.1):
    /// one block per op array, the header's four lines, one line per
    /// opline with the operand `result` names opening it, `T1 = `, when its
    /// kind is `TMP`, `VAR` or `CV`, its constants, variables and jump
    /// targets printed as php prints them, and the `try`/`catch` table.
    #[must_use]
    pub fn dump(&self) -> String {
        crate::dump::unit(self)
    }
}
