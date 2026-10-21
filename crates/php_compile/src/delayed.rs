//! `CG(delayed_oplines_stack)` (`Z/Zend/zend_compile.c:425`): the
//! `zend_op`s `zend_delayed_emit_op` holds back while the rest of a chain
//! of dimensions and properties compiles, pushed one a fetch
//! (`:2458`) and popped together by `zend_delayed_compile_end`, which
//! lays them down after everything the chain's operands compiled to.
//! The stack grows sixteen elements at a time and never shrinks before
//! `shutdown_compiler`, so what a compile leaves the request holding is
//! the most it held at once ([`php_unit::Unit::delayed_fetches`]).
//!
//! Each held-back fetch is one opline of the op array it lands in, and
//! the fetches one chain held lie together in it, each reading the one
//! before: `$a[0][1]` is `FETCH_DIM_R $a, 0` and then
//! `FETCH_DIM_R ~0, 1`. So the most a compile held is read off its op
//! arrays as the longest such stretch. A chain whose dimension is itself a
//! chain (`$a[0][$b[0]]`) held the outer's fetches beneath the inner's,
//! which the stretches count apart, and a `?->` in a chain parts it with
//! its `JMP_NULL` where php's stack keeps the fetches before it; php's
//! count is the larger in those only past sixteen held at once, where the
//! stack's size moves.

use php_unit::{OpArray, Opcode, OperandKind, Opline};

use crate::emit::FETCH_GLOBAL;

/// `ZEND_FETCH_TYPE_MASK` (`Z/Zend/zend_compile.h:1086`), the bits of a
/// fetch by name's `extended_value` that say which table it reads.
const FETCH_TYPE_MASK: u32 = 0xe;

/// The most fetches `zend_delayed_emit_op` held at once while `op_arrays`
/// compiled: one for an assignment to a variable named by a variable,
/// whose `FETCH_W` the assignment writes through, which lays the stack's
/// sixteen, 512 bytes, where the plain one keeps a name `$$n` makes none
/// of:
///
/// ```php
/// $m = memory_get_usage(); eval('$n = "zz"; $o = 1;'); $plain = memory_get_usage() - $m;
/// $m = memory_get_usage(); eval('$n = "zz"; $$n = 1;'); echo memory_get_usage() - $m - $plain, "\n";
/// ```
/// ```expect regime=jit-off
/// 480
/// ```
pub(crate) fn most_held<'a>(op_arrays: impl IntoIterator<Item = &'a OpArray>) -> u32 {
    op_arrays.into_iter().map(most_in).max().unwrap_or(0)
}

/// [`most_held`] of one op array: its longest stretch of held-back fetches,
/// each reading the one before it.
fn most_in(code: &OpArray) -> u32 {
    let mut most = 0;
    let mut run = 0u32;
    let mut before: Option<&Opline> = None;
    for (at, op) in code.oplines.iter().enumerate() {
        let next = code.oplines.get(at + 1);
        let held = match kind(code, op) {
            Held::Always => true,
            // A fetch by name or of a `static` property is held back only
            // as the base of a chain, which the fetch after it reads, or as
            // what an assignment writes, `$$n = 1` (`zend_compile_assign`'s
            // `zend_delayed_compile_var`).
            Held::AsBase => next.is_some_and(|next| {
                reads(next, op)
                    && (kind(code, next) != Held::Never
                        || matches!(
                            next.opcode,
                            Opcode::Assign | Opcode::AssignOp | Opcode::AssignRef
                        ))
            }),
            Held::Never => false,
        };
        run = match (held, before) {
            (false, _) => 0,
            (true, Some(before)) if run > 0 && reads(op, before) => run + 1,
            (true, _) => 1,
        };
        most = most.max(run);
        before = held.then_some(op);
    }
    most
}

/// Whether `op` reads what `before` wrote, its first operand.
fn reads(op: &Opline, before: &Opline) -> bool {
    matches!(before.out_type, OperandKind::Tmp | OperandKind::Var)
        && op.op1_type == before.out_type
        && op.op1 == before.out
}

/// Whether php's compile holds an opline of `op`'s kind back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// Every one: `zend_delayed_compile_dim` and `zend_delayed_compile_prop`
    /// emit each dimension and property fetch delayed, and what an
    /// assignment, an `isset`, an `unset` or an increment makes of the
    /// last one is that fetch's opline. A read of `$GLOBALS['x']` is a
    /// fetch by name `zend_delayed_compile_dim` holds back too.
    Always,
    /// Only where it is a chain's base (`zend_delayed_compile_var`): a
    /// variable fetched by name and a `static` property.
    AsBase,
    Never,
}

/// What php's compile does with an opline like `op` of `code`.
fn kind(code: &OpArray, op: &Opline) -> Held {
    use Opcode::{
        AssignDim, AssignDimOp, AssignObj, AssignObjOp, AssignObjRef, AssignStaticProp,
        AssignStaticPropOp, AssignStaticPropRef, FetchDimFuncArg, FetchDimIs, FetchDimR,
        FetchDimRw, FetchDimUnset, FetchDimW, FetchFuncArg, FetchIs, FetchObjFuncArg, FetchObjIs,
        FetchObjR, FetchObjRw, FetchObjUnset, FetchObjW, FetchR, FetchRw, FetchStaticPropFuncArg,
        FetchStaticPropIs, FetchStaticPropR, FetchStaticPropRw, FetchStaticPropUnset,
        FetchStaticPropW, FetchUnset, FetchW, IssetIsemptyDimObj, IssetIsemptyPropObj, PostDecObj,
        PostIncObj, PreDecObj, PreIncObj, UnsetDim, UnsetObj,
    };
    match op.opcode {
        FetchDimR | FetchDimW | FetchDimRw | FetchDimIs | FetchDimFuncArg | FetchDimUnset
        | FetchObjR | FetchObjW | FetchObjRw | FetchObjIs | FetchObjFuncArg | FetchObjUnset
        | AssignDim | AssignObj | AssignStaticProp | AssignDimOp | AssignObjOp
        | AssignStaticPropOp | AssignObjRef | AssignStaticPropRef | UnsetDim | UnsetObj
        | IssetIsemptyDimObj | IssetIsemptyPropObj | PreIncObj | PreDecObj | PostIncObj
        | PostDecObj => Held::Always,
        FetchR | FetchW | FetchRw | FetchIs | FetchFuncArg | FetchUnset
            if globals_element(code, op) =>
        {
            Held::Always
        }
        FetchR
        | FetchW
        | FetchRw
        | FetchIs
        | FetchFuncArg
        | FetchUnset
        | FetchStaticPropR
        | FetchStaticPropW
        | FetchStaticPropRw
        | FetchStaticPropIs
        | FetchStaticPropFuncArg
        | FetchStaticPropUnset => Held::AsBase,
        _ => Held::Never,
    }
}

/// Whether a fetch by name is `$GLOBALS[…]`'s, `zend_delayed_compile_dim`'s
/// first arm: a name from `EG(symbol_table)` that is no auto-global's,
/// which a read of `$_SERVER` itself is.
fn globals_element(code: &OpArray, op: &Opline) -> bool {
    op.extended_value & FETCH_TYPE_MASK == FETCH_GLOBAL
        && (op.op1_type != OperandKind::Const
            || (code.literals.get(op.op1 as usize))
                .and_then(|name| name.as_str())
                .is_none_or(|name| php_unit::AutoGlobal::of(name.bytes()).is_none()))
}
