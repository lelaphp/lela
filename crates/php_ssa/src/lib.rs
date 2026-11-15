//! php's function-level analysis (ADR-0015, `jit-design.md` §4.6): the CFG,
//! DFG and SSA that `Zend/Optimizer` builds over one op array, built as
//! `zend_jit_op_array_analyze1` builds them on a trace's first pass
//! through the op array, and the types and integer ranges
//! `zend_ssa_inference` infers over them as `zend_jit_op_array_analyze2`
//! runs it, with php's algorithms copied where the order of blocks, phis
//! and SSA variables, and what inference learns of each, is observable in
//! php's own dump (`opcache.jit_debug`'s `ZEND_JIT_DEBUG_SSA`).
//!
//! The crate reads the unit format and the constants an op array carries,
//! and asks its caller, through [`LinkedTable`], for what an op array does
//! not hold: which class a name binds, and what inference reads of a
//! linked class. The compiled tier alone calls it, and holds what it
//! builds once per op array.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]
// php's functions are copied whole, each a walk or a switch over opcodes
// that reads best beside its source in one piece.
#![allow(clippy::too_many_lines)]

mod bits;
mod calls;
mod cfg;
mod dfg;
mod dump;
mod func_info;
mod infer;
mod may_throw;
mod range;
mod ssa;
mod trace;
mod types;
mod update;

use php_macros::EngineState;
use php_unit::decl::{FunctionDecl, HookKind, Type, Visibility};
use php_unit::{OpArray, OperandKind, Opline};

pub use cfg::{Block, Cfg};
pub use ssa::{
    Alias, Constraint, Negative, Phi, PhiKind, RangeConstraint, Ssa, SsaOp, SsaVarInfo,
    is_no_val_use,
};
pub use trace::{Position, TraceFrame};
pub use types::{ClassOf, Range, Returns, Types, VarInfo};

/// A basic block's number, its index in [`Cfg::blocks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, EngineState)]
pub struct BlockId(pub u32);

/// A variable of the frame, `EX_VAR_TO_NUM`: a CV below the op array's
/// `num_cvs`, a temporary past them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, EngineState)]
pub struct Var(pub u32);

/// An SSA variable's number, its index in [`Ssa::vars`]; the first
/// `num_cvs` are each CV's value on entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, EngineState)]
pub struct SsaVar(pub u32);

/// A phi's index in [`Ssa::phis`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, EngineState)]
pub struct PhiId(pub u32);

macro_rules! index {
    ($($id:ident),*) => {$(
        impl $id {
            /// The number as an index.
            #[must_use]
            pub fn index(self) -> usize {
                self.0 as usize
            }

            pub(crate) fn of(n: usize) -> $id {
                $id(u32::try_from(n).unwrap_or(u32::MAX))
            }
        }
    )*};
}
index!(BlockId, Var, SsaVar, PhiId);

/// What `zend_build_cfg` marks a block with, `ZEND_BB_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EngineState)]
#[allow(clippy::struct_excessive_bools)]
pub struct Marks {
    /// The entry.
    pub start: bool,
    /// Entered by falling through from the block before it.
    pub follow: bool,
    /// Entered by a jump.
    pub target: bool,
    /// Without successors.
    pub exit: bool,
    /// Entered after a call or a `yield` returns, `ZEND_BB_ENTRY`.
    pub entry: bool,
    pub try_: bool,
    pub catch: bool,
    pub finally: bool,
    pub finally_end: bool,
    /// Unreachable, but frees a loop variable a reachable block defined.
    pub unreachable_free: bool,
    /// Entered after a `RECV` or `RECV_INIT`.
    pub recv_entry: bool,
    pub loop_header: bool,
    pub irreducible: bool,
    pub reachable: bool,
}

/// What the CFG learns of the whole function, `ZEND_FUNC_*` in
/// `cfg->flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EngineState)]
#[allow(clippy::struct_excessive_bools)]
pub struct Features {
    /// Its variables are reachable by name: `extract`, `compact`,
    /// `get_defined_vars`, `include` or `eval`, a variable variable, or a
    /// file's own code.
    pub indirect_var_access: bool,
    pub has_calls: bool,
    /// It reads its arguments as a list.
    pub vararg: bool,
    pub no_loops: bool,
    pub irreducible: bool,
    pub free_loop_var: bool,
    pub extended_stmt: bool,
    pub extended_fcall: bool,
}

impl Features {
    fn join(&mut self, other: Features) {
        self.indirect_var_access |= other.indirect_var_access;
        self.vararg |= other.vararg;
    }
}

/// The classes an op array's code can name while it is analysed, as
/// the caller holds them linked: `zend_optimizer_get_class_entry`'s
/// lookup and what inference reads of a class entry.
pub trait LinkedTable {
    /// A linked class, as the caller holds one.
    type Class: Copy + PartialEq;

    /// The class `lcname`, in lower case, binds for code of the file the
    /// op array was compiled from, as `EG(class_table)` holds it when php
    /// builds the analysis: one of the engine's own, or one declared in
    /// that same file; none otherwise.
    fn class(&self, lcname: &[u8]) -> Option<Self::Class>;

    /// The engine's own class `lcname`, in lower case, which a declared
    /// type binds alone.
    fn engine_class(&self, lcname: &[u8]) -> Option<Self::Class>;

    /// The class the op array is declared in, `op_array->scope`, which a
    /// name binds that no other class answers to.
    fn scope(&self) -> Option<Self::Class>;

    /// The class's name as declared.
    fn name(&self, class: Self::Class) -> &[u8];

    /// `ce->parent`.
    fn parent(&self, class: Self::Class) -> Option<Self::Class>;

    /// `instanceof_function(class, of)`: `of` is the class, a parent of
    /// it, or an interface it implements.
    fn instance_of(&self, class: Self::Class, of: Self::Class) -> bool;

    /// `ZEND_ACC_FINAL`.
    fn is_final(&self, class: Self::Class) -> bool;

    /// `ZEND_ACC_TRAIT`.
    fn is_trait(&self, class: Self::Class) -> bool;

    /// What making, reading and writing an object of the class runs
    /// besides the standard handlers.
    fn object(&self, class: Self::Class) -> ObjectShape;

    /// `zend_get_property_info(class, name, silent)` as code of `scope`
    /// sees it: the property a fetch of `name` on an object of `class`
    /// finds; none where it finds none or one `scope` may not reach.
    fn property(
        &self,
        class: Self::Class,
        name: &[u8],
        scope: Option<Self::Class>,
    ) -> Option<Property<'_, Self::Class>>;

    /// The class constant `name` of `class` as `zend_fetch_class_const_info`
    /// finds it in `ce->constants_table`; none for a trait's.
    fn constant(&self, class: Self::Class, name: &[u8]) -> Option<ClassConstant<'_, Self::Class>>;

    /// The function `lcname`, in lower case, names for the op array's
    /// code, as `zend_optimizer_get_called_func` finds it with `script`
    /// null: `EG(function_table)`'s native, or its user function where it
    /// was declared in the op array's own file
    /// (`zend_optimizer_ignore_function`); none otherwise.
    fn function(&self, lcname: &[u8]) -> Option<Callee<'_, Self::Class>>;

    /// The method `lcname`, in lower case, of `class`'s function table,
    /// `zend_hash_find_ptr(&ce->function_table, lcname)`.
    fn method(&self, class: Self::Class, lcname: &[u8]) -> Option<Callee<'_, Self::Class>>;

    /// The native row `index` of the engine's frameless table is,
    /// `ZEND_FLF_FUNC`, which a `FRAMELESS_ICALL_*` whose `extended_value`
    /// is `index` calls.
    fn frameless(&self, index: u32) -> Option<Callee<'_, Self::Class>>;

    /// The hook of `kind` the parent of the op array's class holds for
    /// its property `name`, where the parent's property is not private:
    /// `prop_info->hooks[kind]` as `INIT_PARENT_PROPERTY_HOOK_CALL` finds
    /// it.
    fn parent_hook(&self, name: &[u8], kind: HookKind) -> Option<Callee<'_, Self::Class>>;
}

/// A function a call reaches, `zend_function` as `zend_analyze_calls`
/// and `zend_get_func_info` read it.
#[derive(Debug, Clone, Copy)]
pub struct Callee<'a, C> {
    /// `common.function_name`.
    pub name: &'a [u8],
    /// Its declared return type, `arg_info[-1]`; none where it declares
    /// none.
    pub return_type: Option<&'a Type>,
    /// `ZEND_ACC_RETURN_REFERENCE`.
    pub returns_by_ref: bool,
    /// `ZEND_ACC_GENERATOR`.
    pub generator: bool,
    /// A method's class and flags; none for a function.
    pub method: Option<Method<C>>,
    pub body: Body<C>,
}

/// A method's `common.scope` and the `fn_flags` the call analysis reads.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct Method<C> {
    /// The class that declares it.
    pub scope: C,
    pub visibility: Visibility,
    /// `ZEND_ACC_FINAL`.
    pub is_final: bool,
    /// `ZEND_ACC_ABSTRACT`.
    pub is_abstract: bool,
    /// The tentative bit of a native's declared return type.
    pub tentative_return: bool,
}

/// What a callee is, `callee_func->type`, and what php holds of a user
/// function's return.
#[derive(Debug, Clone, Copy)]
pub enum Body<C> {
    /// `ZEND_INTERNAL_FUNCTION`.
    Native,
    /// The op array being analysed itself, whose `func_info->return_info`
    /// php resets as the analysis starts.
    Analysed,
    /// Another op array, with the `func_info->return_info` its last
    /// analysis left; none where none was built.
    User(Option<Returns<C>>),
}

/// What an object of a class does besides `zend_std_*`'s handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ObjectShape {
    /// `ce->constructor`.
    pub constructor: bool,
    /// `ce->__get`.
    pub get: bool,
    /// `ce->__set`.
    pub set: bool,
    /// `ce->create_object` is unset and its object handlers are php's
    /// standard ones, as a user class not extending an engine one has.
    pub standard: bool,
}

/// `zend_property_info` as inference reads it.
#[derive(Debug, Clone, Copy)]
pub struct Property<'a, C> {
    /// `ZEND_ACC_STATIC`.
    pub is_static: bool,
    /// Its declared type; none where it declares none.
    pub ty: Option<&'a Type>,
    /// The class that declares it, `prop_info->ce`, whose name a `self`
    /// of its type is.
    pub declared_in: C,
}

/// `zend_class_constant` as inference reads it.
#[derive(Debug, Clone, Copy)]
pub struct ClassConstant<'a, C> {
    /// Its declared type; none where it declares none.
    pub ty: Option<&'a Type>,
    /// `ZEND_ACC_PUBLIC` among its flags.
    pub is_public: bool,
    /// `ZEND_ACC_DEPRECATED`.
    pub is_deprecated: bool,
    /// The class that declares it.
    pub declared_in: C,
}

/// `zend_optimizer_get_class_entry` with `script` null: the class `lcname`
/// binds for `function`'s code, or its scope where that is the class's
/// name.
fn class_entry<T: LinkedTable>(
    function: &Function<'_>,
    table: &T,
    lcname: &[u8],
) -> Option<T::Class> {
    table.class(lcname).or_else(|| {
        (function.scope)
            .filter(|scope| scope.eq_ignore_ascii_case(lcname))
            .and_then(|_| table.scope())
    })
}

/// The class the constant at `n` of `function`'s literals names, by the
/// lower-cased name the compiler wrote after it.
fn class_named<T: LinkedTable>(function: &Function<'_>, table: &T, n: u32) -> Option<T::Class> {
    let literals = &function.body.literals;
    literals.get(n as usize)?.as_str()?;
    let lcname = literals.get(n as usize + 1)?.str_bytes()?;
    class_entry(function, table, lcname)
}

/// `zend_optimizer_get_class_entry_from_op1`: the class `op`'s first
/// operand names, a constant name, or `self` or a final class's `static`
/// as the op array's scope.
fn class_of_op1<T: LinkedTable>(
    function: &Function<'_>,
    table: &T,
    op: &Opline,
) -> Option<T::Class> {
    match op.op1_type {
        OperandKind::Const => class_named(function, table, op.op1),
        OperandKind::Unused => {
            let scope = table.scope().filter(|s| !table.is_trait(*s))?;
            let fetch = op.op1 & update::FETCH_CLASS_MASK;
            (fetch == update::FETCH_CLASS_SELF
                || (fetch == update::FETCH_CLASS_STATIC && table.is_final(scope)))
            .then_some(scope)
        }
        _ => None,
    }
}

/// One op array and what php reads beside it.
#[derive(Debug, Clone, Copy)]
pub struct Function<'a> {
    pub body: &'a OpArray,
    /// The declaration it is the body of, `op_array->function_name`,
    /// `arg_info` and `fn_flags`; none for a file's own code.
    pub decl: Option<&'a FunctionDecl>,
    /// The name of the class it is declared in, `op_array->scope`'s.
    pub scope: Option<&'a [u8]>,
}

impl<'a> Function<'a> {
    /// `op_array->function_name`.
    #[must_use]
    pub fn name(&self) -> Option<&'a [u8]> {
        self.decl.map(|decl| &*decl.name.0)
    }

    /// `op_array->num_args`, the declared parameters but a variadic one.
    #[must_use]
    pub fn num_args(&self) -> u32 {
        let n = self.decl.map_or(0, |decl| {
            decl.parameters.iter().filter(|p| !p.variadic).count()
        });
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// `ZEND_ACC_GENERATOR`.
    #[must_use]
    pub fn generator(&self) -> bool {
        self.decl.is_some_and(|decl| decl.is_generator)
    }

    /// The function as a call to it reads it, `(zend_function *)
    /// op_array`; none for a file's own code.
    fn callee<C>(&self) -> Option<Callee<'a, C>> {
        let decl = self.decl?;
        Some(Callee {
            name: &decl.name.0,
            return_type: decl.return_type.as_ref(),
            returns_by_ref: decl.returns_by_ref,
            generator: decl.is_generator,
            method: None,
            body: Body::Analysed,
        })
    }
}

/// `opcache.jit`'s optimization level, its `O` digit, from which php
/// analyses an op array a trace passes through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// `ZEND_JIT_LEVEL_OPT_FUNC`: the op array alone.
    Func,
    /// `ZEND_JIT_LEVEL_OPT_FUNCS` and above: with the calls it makes and
    /// its declared return type.
    Funcs,
}

/// What `zend_jit_trace_build_ssa` leaves: the CFG always, the SSA with
/// what inference learned where php builds one, and what the function
/// returns.
#[derive(Debug, Clone, PartialEq, EngineState)]
pub struct Analysis<C> {
    pub cfg: Cfg,
    /// None for a file's own code, a generator, a function with a `try`,
    /// or one whose variables are reachable by name, as php declines them.
    pub ssa: Option<Ssa<C>>,
    /// `func_info->return_info`.
    pub returns: Returns<C>,
}

/// `zend_jit_trace_build_ssa` at `level`: the CFG, then the SSA, its
/// use-def chains, its false dependencies and its SCCs where php builds
/// them, and inference over them; at [`Level::Funcs`], with the calls the
/// op array makes (`zend_analyze_calls`) and what its declared return
/// type says it returns (`zend_init_func_return_info`) read beside it.
/// None where php's analysis fails, for a
/// function of more than 100,000 blocks or one whose blocks times
/// variables pass 4,194,304, or where inference narrows a type, which
/// php neither compiles from nor dumps.
pub fn analyse<T: LinkedTable>(
    function: &Function<'_>,
    table: &T,
    level: Level,
) -> Option<Analysis<T::Class>> {
    let body = function.body;
    let mut cfg = cfg::build(body, function.name().is_some())?;
    if function.name().is_none() {
        cfg.features.indirect_var_access = true;
    }
    let funcs = level >= Level::Funcs;
    let mut returns = if funcs {
        func_info::init_return_info(function, table)
    } else {
        Returns::default()
    };
    if !body.try_catch.is_empty() || function.generator() || cfg.features.indirect_var_access {
        return Some(Analysis {
            cfg,
            ssa: None,
            returns,
        });
    }
    let mut ssa = ssa::build(function, &cfg, table)?;
    let calls = funcs.then(|| calls::analyse(function, table)).flatten();
    infer::infer(
        function,
        &cfg,
        &mut ssa,
        table,
        calls.as_ref(),
        &mut returns,
    )?;
    Some(Analysis {
        cfg,
        ssa: Some(ssa),
        returns,
    })
}

#[cfg(test)]
mod tests {
    use php_heap::EngineState;

    use crate::Analysis;

    fn held<T: EngineState>() {}

    /// An analysis is engine state for every class type `C` that is, each
    /// field held by the derive where it is declared: the tier
    /// keeps one per op array in the epoch's state.
    fn analysis_of<C: EngineState>() {
        held::<Analysis<C>>();
    }

    #[test]
    fn an_analysis_over_engine_state_is_engine_state() {
        analysis_of::<u32>();
    }
}
