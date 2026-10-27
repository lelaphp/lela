//! php's function-level analysis (ADR-0015, `jit-design.md` §4.6): the CFG,
//! DFG and SSA that `Zend/Optimizer` builds over one op array, built as
//! `zend_jit_op_array_analyze1` builds them on a trace's first pass
//! through the op array, with php's algorithms copied where the order of
//! blocks, phis and SSA variables is observable in php's own dump
//! (`opcache.jit_debug`'s `ZEND_JIT_DEBUG_SSA`).
//!
//! The crate reads the unit format and the constants an op array carries,
//! and asks its caller, through [`LinkedTable`], for the one thing an op
//! array does not hold: which class a name binds. The compiled tier alone
//! calls it, and holds what it builds once per op array.
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
mod cfg;
mod dfg;
mod dump;
mod ssa;

use php_unit::OpArray;

pub use cfg::{Block, Cfg};
pub use ssa::{Alias, Constraint, Negative, Phi, PhiKind, RangeConstraint, Ssa, SsaOp, SsaVarInfo};

/// A basic block's number, its index in [`Cfg::blocks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BlockId(pub u32);

/// A variable of the frame, `EX_VAR_TO_NUM`: a CV below the op array's
/// `num_cvs`, a temporary past them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Var(pub u32);

/// An SSA variable's number, its index in [`Ssa::vars`]; the first
/// `num_cvs` are each CV's value on entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SsaVar(pub u32);

/// A phi's index in [`Ssa::phis`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

/// The class names an op array's code can bind while it is analysed:
/// `zend_optimizer_get_class_entry`'s lookup, which the caller answers
/// from the classes linked so far.
pub trait LinkedTable {
    /// A linked class, as the caller holds one.
    type Class: Copy + PartialEq;

    /// The class `lcname`, in lower case, binds for code of the file the
    /// op array was compiled from: one of the engine's own, or one
    /// declared in that same file; none otherwise.
    fn class(&self, lcname: &[u8]) -> Option<Self::Class>;

    /// The class the op array is declared in, `op_array->scope`, which a
    /// name binds that no other class answers to.
    fn scope(&self) -> Option<Self::Class>;

    /// The class's name as declared.
    fn name(&self, class: Self::Class) -> &[u8];
}

/// One op array and what php reads beside it.
#[derive(Debug, Clone, Copy)]
pub struct Function<'a> {
    pub body: &'a OpArray,
    /// `op_array->function_name`; none for a file's own code.
    pub name: Option<&'a [u8]>,
    /// The name of the class it is declared in, `op_array->scope`'s.
    pub scope: Option<&'a [u8]>,
    /// `op_array->num_args`, the declared parameters but a variadic one.
    pub num_args: u32,
    /// `ZEND_ACC_GENERATOR`.
    pub generator: bool,
}

/// What `zend_jit_op_array_analyze1` leaves: the CFG always, and the SSA
/// where php builds one.
#[derive(Debug, Clone, PartialEq)]
pub struct Analysis<C> {
    pub cfg: Cfg,
    /// None for a file's own code, a generator, a function with a `try`,
    /// or one whose variables are reachable by name, as php declines them.
    pub ssa: Option<Ssa<C>>,
}

/// `zend_jit_op_array_analyze1` at `opcache.jit`'s optimization level 3 or
/// above: the CFG, then the SSA, its use-def chains, its false
/// dependencies and its SCCs where php builds them. None where php's
/// analysis fails, for a function of more than 100,000 blocks or one whose
/// blocks times variables pass 4,194,304, which php neither compiles from
/// its SSA nor dumps.
pub fn analyse<T: LinkedTable>(function: &Function<'_>, table: &T) -> Option<Analysis<T::Class>> {
    let body = function.body;
    let mut cfg = cfg::build(body, function.name.is_some())?;
    if function.name.is_none() {
        cfg.features.indirect_var_access = true;
    }
    if !body.try_catch.is_empty() || function.generator || cfg.features.indirect_var_access {
        return Some(Analysis { cfg, ssa: None });
    }
    let ssa = ssa::build(function, &cfg, table)?;
    Some(Analysis {
        cfg,
        ssa: Some(ssa),
    })
}
