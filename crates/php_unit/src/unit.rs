//! A unit (design §4.7): one compiled file, immutable once published, its
//! declarations beside their op arrays.

use std::ops::Deref;
use std::sync::{Arc, OnceLock, Weak};

use crate::decl::{ClassDecl, ConstDecl, FunctionDecl, HookKind, Name};
use crate::op::OpArray;

/// The unit a constant expression's closure belongs to
/// ([`crate::decl::ConstExpr::Closure`]), as the engine shares it:
/// empty while the unit compiles, and bound once to the unit's shared
/// copy by [`Unit::published`], which is what php's `ZEND_AST_OP_ARRAY`
/// reaches directly, its `zend_op_array` pointer. Every closure node of
/// a unit holds a share of the unit's one cell ([`UnitCell::share`]);
/// the cell holds the unit weakly, so nothing keeps a unit alive but its
/// sharers. A unit no closure shares, nearly every one, holds an empty
/// cell, which costs its compile nothing.
#[derive(Clone, Default)]
pub struct UnitCell(Option<Arc<OnceLock<Weak<Unit>>>>);

impl UnitCell {
    /// The unit the cell was bound to, while it is shared; none before
    /// [`Unit::published`] and after the last share is gone.
    #[must_use]
    pub fn unit(&self) -> Option<PublishedUnit> {
        self.0
            .as_ref()?
            .get()
            .and_then(Weak::upgrade)
            .map(PublishedUnit)
    }

    /// A share of the cell for a closure of the unit, the cell laid down
    /// on the first.
    #[must_use]
    pub fn share(&mut self) -> UnitCell {
        UnitCell(Some(Arc::clone(self.0.get_or_insert_default())))
    }
}

/// Two nodes name one cell, which is one unit's.
impl PartialEq for UnitCell {
    fn eq(&self, other: &UnitCell) -> bool {
        match (&self.0, &other.0) {
            (Some(one), Some(other)) => Arc::ptr_eq(one, other),
            (None, None) => true,
            _ => false,
        }
    }
}

impl std::fmt::Debug for UnitCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            if self.0.as_ref().is_some_and(|cell| cell.get().is_some()) {
                "UnitCell(bound)"
            } else {
                "UnitCell(unbound)"
            },
        )
    }
}

/// A compiled PHP program: the main op array, the functions with their
/// bodies at the same index, the classes and the constants declared,
/// the bodies of the classes' methods, and the diagnostics compiling it
/// raised.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Unit {
    pub main: OpArray,
    pub functions: Vec<(FunctionDecl, OpArray)>,
    pub classes: Vec<ClassDecl>,
    /// The runtime definition key of every class of `classes` that no
    /// name bound while the file compiled, beside the class's index:
    /// php's `CG(class_table)` holds such a class under the key
    /// `zend_build_runtime_definition_key` made, `\0`, the lowered name,
    /// the file, `:`, the declaration's first line, `$` and how many keys
    /// the file made before it in hex, and the `DECLARE_CLASS` that binds
    /// it where it was written carries the key in the constant after its
    /// `op1`. So two classes of
    /// one name, one in each arm of an `if`, are two keys. A class no key
    /// names was bound early and is bound when the unit loads.
    pub runtime_keys: Vec<(Name, u32)>,
    /// Every top-level function and class of the file, in the order
    /// its compile met them: what php's compiler declares, and tells
    /// the observers of, as `zend_compile_func_decl` and
    /// `zend_compile_class_decl` finish each (`Z/Zend/zend_compile.c:8627`,
    /// `:9386`), which no line tells apart where two end on one.
    pub top_level: Vec<TopLevel>,
    pub constants: Vec<ConstDecl>,
    /// The op array of each method a class of `classes` declares with a
    /// body, php's `ce->function_table` laid beside the declaration
    /// model rather than inside it: a `MethodDecl` is what a native
    /// declares too, and a native's body is Rust. In the order
    /// `zend_compile_class_decl` compiled them, class by class.
    pub methods: Vec<MethodBody>,
    /// The op array of each hook a property of a class of `classes`
    /// declares with a body, php's `prop_info->hooks[kind]` laid beside
    /// the declaration model as a method's is: in the order
    /// `zend_compile_property_hooks` compiled them.
    pub hooks: Vec<HookBody>,
    /// Each class of `classes` by its index, in the order its
    /// declaration completed, after its body, which is where php's
    /// `zend_compile_class_decl` adds it to `CG(class_table)`
    /// (`Z/Zend/zend_compile.c:9348`): after every class declared inside
    /// one of its methods or hooks. A unit built by hand that leaves it
    /// empty is read in declaration order.
    pub completed: Vec<u32>,
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
    /// Where the file's data begins, the byte after the `;` of the
    /// `__halt_compiler();` it stops at, for a file that stops at one:
    /// what `zend_compile_halt_compiler` registers as
    /// `__COMPILER_HALT_OFFSET__` under the file's name
    /// (`Z/Zend/zend_compile.c:9804`), and what opcache keeps beside a
    /// cached file as `compiler_halt_offset` and registers again on a
    /// hit (`Z/ext/opcache/zend_accelerator_util_funcs.c:380`). The
    /// engine registers it where it raises [`Unit::diagnostics`].
    pub halt_offset: Option<i64>,
    /// The cell every closure of the unit's constant expressions reaches
    /// the unit through, bound when the unit is published.
    pub cell: UnitCell,
}

/// A unit as [`Unit::published`], its one constructor, leaves it: every
/// opline's handler resolved, its [`UnitCell`] bound, and shared,
/// read-only, by every request and frame that serves it. It is the one
/// form of a unit the engine reads a frame's op array through: a file's
/// frame is pushed for one and holds it, and a function's frame reads
/// its op array through the function table, which holds the unit the
/// function was declared from as one. `execute` enters only a frame, so
/// no opline reaches the dispatch with its handler `Unresolved` (design
/// §5.7, #2903). A [`Unit`] is compiled, dumped,
/// cached and read back, and served only once it is published. Design
/// §5.14's `Published`, which the unit cache stores, is the engine's
/// form of a cached unit; an `eval`'s unit is a published unit too.
///
/// A unit is not a published one, so a file's frame push, which takes
/// one (`php_engine`'s `Request::push_frame`), is handed none:
///
/// ```compile_fail
/// fn pushed(_: php_unit::PublishedUnit) {}
/// pushed(php_unit::Unit::default());
/// ```
///
/// No share of a unit becomes a published one but the one
/// [`Unit::published`] makes:
///
/// ```compile_fail
/// let shared = std::sync::Arc::new(php_unit::Unit::default());
/// let published = php_unit::PublishedUnit(shared);
/// ```
///
/// ```compile_fail
/// let shared = std::sync::Arc::new(php_unit::Unit::default());
/// let published = php_unit::PublishedUnit::from(shared);
/// ```
///
/// And a published unit's oplines are read and never written, so none is
/// put back to `Unresolved`:
///
/// ```compile_fail
/// let mut published = php_unit::Unit::default().published();
/// published.main.oplines[0].handler = php_unit::Handler::Unresolved;
/// ```
///
/// What each of those is refused for, done the one way:
///
/// ```
/// use php_unit::{Handler, OpArray, Opline, Unit};
/// let unit = Unit {
///     main: OpArray { oplines: vec![Opline::default()], ..OpArray::default() },
///     ..Unit::default()
/// };
/// assert_eq!(unit.main.oplines[0].handler, Handler::Unresolved);
/// let published = unit.published();
/// assert_eq!(published.main.oplines[0].handler, Handler::Nop);
/// ```
#[derive(Debug, Clone)]
pub struct PublishedUnit(Arc<Unit>);

impl Deref for PublishedUnit {
    type Target = Unit;

    fn deref(&self) -> &Unit {
        &self.0
    }
}

impl PublishedUnit {
    /// Whether the two are one publication, as [`Arc::ptr_eq`] answers.
    #[must_use]
    pub fn ptr_eq(this: &PublishedUnit, other: &PublishedUnit) -> bool {
        Arc::ptr_eq(&this.0, &other.0)
    }

    /// Where the published unit stands, its identity while any share of
    /// it is held, as [`Arc::as_ptr`] answers.
    #[must_use]
    pub fn as_ptr(this: &PublishedUnit) -> *const Unit {
        Arc::as_ptr(&this.0)
    }

    /// A share that keeps the unit's identity and not the unit, as
    /// [`Arc::downgrade`] makes: what a holder keeps that must not keep
    /// the unit alive, to learn when the last share is gone.
    #[must_use]
    pub fn downgrade(this: &PublishedUnit) -> WeakUnit {
        WeakUnit(Arc::downgrade(&this.0))
    }
}

/// A published unit's identity without the unit
/// ([`PublishedUnit::downgrade`]): how many shares of it stand, and where
/// it stands. It never gives the unit back, so no `Arc<Unit>` of a
/// published unit is held outside this crate, and none can be made the
/// only share and written through:
///
/// ```compile_fail
/// let published = php_unit::Unit::default().published();
/// let weak = php_unit::PublishedUnit::downgrade(&published);
/// drop(published);
/// let shared = weak.upgrade();
/// ```
#[derive(Debug, Clone)]
pub struct WeakUnit(Weak<Unit>);

impl WeakUnit {
    /// How many shares of the published unit stand, none once it is gone,
    /// as [`Weak::strong_count`] answers.
    #[must_use]
    pub fn strong_count(&self) -> usize {
        self.0.strong_count()
    }

    /// Where the published unit stands, what [`PublishedUnit::as_ptr`]
    /// answers for a share of it, as [`Weak::as_ptr`] answers.
    #[must_use]
    pub fn as_ptr(&self) -> *const Unit {
        self.0.as_ptr()
    }
}

/// One of [`Unit::top_level`]: a function of [`Unit::functions`] or a
/// class of [`Unit::classes`], by its index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopLevel {
    Function(u32),
    Class(u32),
}

impl Unit {
    /// The unit shared, as the engine publishes every unit it compiles,
    /// with every opline's handler resolved ([`crate::Handler`]) and its
    /// [`UnitCell`] bound to the shared copy, so a closure its constant
    /// expressions hold can be made where they are evaluated: the one
    /// constructor of a [`PublishedUnit`].
    #[must_use]
    pub fn published(mut self) -> PublishedUnit {
        for code in self.op_arrays_mut() {
            code.resolve();
        }
        let shared = Arc::new(self);
        // A unit is published once; a clone published again keeps the
        // first binding, the cell being the clone's too.
        if let Some(cell) = &shared.cell.0 {
            cell.set(Arc::downgrade(&shared)).ok();
        }
        PublishedUnit(shared)
    }

    /// Every op array's tables shrunk to what they hold, as opcache
    /// copies a persisted unit into shared memory at its exact size
    /// (`zend_persist_op_array_ex`): what a unit held for more than one
    /// request pays once, where the engine publishes it, and a unit one
    /// request compiles for itself, an `eval`'s, never pays.
    pub fn compact(&mut self) {
        for code in self.op_arrays_mut() {
            code.compact();
        }
    }

    /// Every op array of the unit: its main one, its functions', the
    /// ones no name binds, its methods' and its property hooks'.
    fn op_arrays_mut(&mut self) -> impl Iterator<Item = &mut OpArray> {
        let Unit {
            main,
            functions,
            methods,
            hooks,
            dynamic,
            ..
        } = self;
        std::iter::once(main)
            .chain(functions.iter_mut().map(|(_, body)| body))
            .chain(dynamic.iter_mut().map(|(_, body)| body))
            .chain(methods.iter_mut().map(|method| &mut method.body))
            .chain(hooks.iter_mut().map(|hook| &mut hook.body))
    }
}

/// An auto-global that makes a variable: one of the names
/// `php_startup_auto_globals` registers (`Z/main/php_variables.c:975`),
/// or `$_SESSION`, which `PHP_MINIT_FUNCTION(session)` registers with no
/// callback to make it (`Z/ext/session/session.c:2966`) and the session
/// binds when it starts; no CV holds one, and the compiler fetches it by
/// name from `EG(symbol_table)` (`ZEND_FETCH_GLOBAL`). `$GLOBALS` is
/// registered too (`Z/Zend/zend.c:1060`) and makes no variable; the
/// compiler reads it by oplines of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutoGlobal {
    Get,
    Post,
    Cookie,
    Server,
    Env,
    Request,
    Files,
    Session,
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
            b"_SESSION" => AutoGlobal::Session,
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
            AutoGlobal::Session => b"_SESSION",
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

/// One hook's body: which class of [`Unit::classes`], which of its
/// `properties` and which of that property's hooks the op array is the
/// body of.
#[derive(Debug, Clone, PartialEq)]
pub struct HookBody {
    pub class: u32,
    pub property: u32,
    pub kind: HookKind,
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

impl OpArray {
    /// The op array as phpdbg's `print` writes it,
    /// `zend_dump_op_array(op_array, ZEND_DUMP_LINE_NUMBERS, NULL, NULL)`:
    /// a blank line, `name`, the counts with `args` the declared
    /// parameters less a variadic one, the file with the line range,
    /// then each opline opened by `L` and its line, and the `try`/`catch`
    /// table. `frameless` is as [`Unit::dump`] takes it.
    #[must_use]
    pub fn dump_lines(&self, name: &[u8], args: usize, frameless: &[Name]) -> Vec<u8> {
        crate::dump::lines(name, args, self, frameless)
    }
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
    /// a unit that calls none is dumped over an empty table. The dump is
    /// bytes, as php's is: a name, a jump table's key and the file name
    /// print as the bytes they are, whatever encoding they are in.
    #[must_use]
    pub fn dump(&self, frameless: &[Name]) -> Vec<u8> {
        crate::dump::unit(self, frameless)
    }
}
