//! A unit (design §4.7): one compiled file, immutable once published, its
//! declarations beside their op arrays.

use crate::decl::{ClassDecl, ConstDecl, FunctionDecl, Name};
use crate::op::OpArray;

/// A compiled PHP program: the main op array, the functions with their
/// bodies at the same index, the classes and the constants declared,
/// the bodies of the classes' methods, and the diagnostics compiling it
/// raised.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Unit {
    pub main: OpArray,
    pub functions: Vec<(FunctionDecl, OpArray)>,
    pub classes: Vec<ClassDecl>,
    pub constants: Vec<ConstDecl>,
    /// The op array of each method a class of `classes` declares with a
    /// body, php's `ce->function_table` laid beside the declaration
    /// model rather than inside it: a `MethodDecl` is what a native
    /// declares too, and a native's body is Rust. In the order
    /// `zend_compile_class_decl` compiled them, class by class.
    pub methods: Vec<MethodBody>,
    /// Every function of the file that no name binds while it compiles:
    /// one written inside a block, a loop or another body, which php
    /// compiles with `toplevel` false and binds with the
    /// `DECLARE_FUNCTION` opline standing where it was written
    /// (`zend_begin_func_decl`). php keeps each as a pointer in the
    /// declaring op array's `dynamic_func_defs`; the unit keeps them in
    /// one table and an op array keeps indices into it
    /// ([`OpArray::dynamic_func_defs`]), in the order
    /// `zend_add_dynamic_func_def` made them.
    pub dynamic: Vec<(FunctionDecl, OpArray)>,
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
    /// Each auto-global the file names, once, in the order the compiler
    /// first met it: what compiling the file arms. php's compiler arms
    /// one as it meets the name (`zend_is_auto_global`), which is when
    /// one made just in time is laid into `EG(symbol_table)`, and opcache
    /// keeps the same fact beside a cached file as its
    /// `ping_auto_globals_mask` (`Z/ext/opcache/ZendAccelerator.c:1861`)
    /// and arms each again on a hit. The engine arms each where it raises
    /// [`Unit::diagnostics`].
    pub arms: Vec<AutoGlobal>,
}

/// An auto-global that makes a variable: one of the names
/// `php_startup_auto_globals` registers (`Z/main/php_variables.c:975`),
/// which no CV holds and the compiler fetches by name from
/// `EG(symbol_table)` (`ZEND_FETCH_GLOBAL`). `$GLOBALS` is registered too
/// (`Z/Zend/zend.c:1060`) and makes no variable; the compiler reads it by
/// oplines of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutoGlobal {
    Get,
    Post,
    Cookie,
    Server,
    Env,
    Request,
    Files,
}

impl AutoGlobal {
    /// The one a variable's name is, `_SERVER` for `$_SERVER`; `None`
    /// for every other name, `GLOBALS` among them.
    #[must_use]
    pub fn of(name: &[u8]) -> Option<AutoGlobal> {
        Some(match name {
            b"_GET" => AutoGlobal::Get,
            b"_POST" => AutoGlobal::Post,
            b"_COOKIE" => AutoGlobal::Cookie,
            b"_SERVER" => AutoGlobal::Server,
            b"_ENV" => AutoGlobal::Env,
            b"_REQUEST" => AutoGlobal::Request,
            b"_FILES" => AutoGlobal::Files,
            _ => return None,
        })
    }

    /// The variable's name without its `$`.
    #[must_use]
    pub fn name(self) -> &'static [u8] {
        match self {
            AutoGlobal::Get => b"_GET",
            AutoGlobal::Post => b"_POST",
            AutoGlobal::Cookie => b"_COOKIE",
            AutoGlobal::Server => b"_SERVER",
            AutoGlobal::Env => b"_ENV",
            AutoGlobal::Request => b"_REQUEST",
            AutoGlobal::Files => b"_FILES",
        }
    }
}

/// One method's body: which class of [`Unit::classes`] and which of its
/// `methods` the op array is the body of, by index.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodBody {
    pub class: u32,
    pub method: u32,
    pub body: OpArray,
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
    /// one block per op array, the main one, the functions and then each
    /// class's methods, the header's four lines, one line per
    /// opline with the operand `result` names opening it, `T1 = `, when its
    /// kind is `TMP`, `VAR` or `CV`, its constants, variables and jump
    /// targets printed as php prints them, then the live-range table and
    /// the `try`/`catch` table. `frameless` is `zend_flf_functions` by
    /// name, in the order the bundle's natives registered their
    /// frameless entries: a `FRAMELESS_ICALL_*` prints the name its
    /// `extended_value` indexes, `FRAMELESS_ICALL_2(str_contains)`, and
    /// a unit that calls none is dumped over an empty table.
    #[must_use]
    pub fn dump(&self, frameless: &[Name]) -> String {
        crate::dump::unit(self, frameless)
    }
}
