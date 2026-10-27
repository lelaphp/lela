//! What a trace's SSA reads of php's inference, as
//! `zend_jit_trace_build_tssa` calls it over the trace
//! (`Z/ext/opcache/jit/zend_jit_trace.c:2270-2480`): `zend_update_type_info`
//! and `zend_inference_propagate_range` over a trace's SSA, whose ops
//! follow the trace and whose oplines come from each op array it passes
//! through, `ssa_opcodes` naming each one's place. The trace's SSA is the
//! tier's to build; what it learns of each op is php's, in the one
//! function that learns it for an op array's own SSA (`jit-design.md`
//! §3.2).

use crate::bits::Bits;
use crate::calls::{self, Calls};
use crate::infer::Infer;
use crate::{Cfg, Function, Level, LinkedTable, Range, Ssa, SsaVar, Types};

/// Where one op of a trace's SSA stands, `ssa_opcodes[i]`: the op array
/// it is of, by a number the tier gives each op array of the trace, and
/// the opline's place in it. The op after an assignment that carries an
/// `OP_DATA` is its `OP_DATA`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub array: u32,
    pub opline: u32,
}

/// One op array a trace passes through, as php's inference of the
/// trace's ops reads it: the op array, the linked table its analysis
/// binds, and at [`Level::Funcs`] the calls it makes, `func_info->call_map`,
/// which types what a call returns.
pub struct TraceFrame<'a, T: LinkedTable> {
    function: &'a Function<'a>,
    table: &'a T,
    calls: Option<Calls<'a, T::Class>>,
    cfg: Cfg,
}

impl<'a, T: LinkedTable> TraceFrame<'a, T> {
    /// The frame of `function` at `level`, as `zend_jit_trace_build_ssa`
    /// analysed it.
    pub fn new(function: &'a Function<'a>, table: &'a T, level: Level) -> TraceFrame<'a, T> {
        TraceFrame {
            function,
            table,
            calls: (level >= Level::Funcs)
                .then(|| calls::analyse(function, table))
                .flatten(),
            cfg: Cfg::default(),
        }
    }

    /// Inference over `tssa` at the trace's op `at`, its
    /// `positions` php's `ssa_opcodes`, with what it learned so far in
    /// `tssa.info`.
    fn infer<R>(
        &self,
        tssa: &mut Ssa<T::Class>,
        positions: &[Position],
        run: impl FnOnce(&mut Infer<'_, T>) -> R,
    ) -> R {
        let info = std::mem::take(&mut tssa.info);
        let mut cx = Infer {
            function: self.function,
            body: self.function.body,
            cfg: &self.cfg,
            ssa: &*tssa,
            table: self.table,
            calls: self.calls.as_ref(),
            info,
            positions: Some(positions),
        };
        let out = run(&mut cx);
        tssa.info = cx.info;
        out
    }

    /// `zend_update_type_info` at the trace's op `at`, writing what each
    /// variable the op defines holds into `tssa.info`; false where php's
    /// would narrow a type it already wrote, which it asserts never
    /// happens.
    pub fn update_type_info(
        &self,
        tssa: &mut Ssa<T::Class>,
        positions: &[Position],
        at: usize,
    ) -> bool {
        self.infer(tssa, positions, |cx| {
            cx.update(at, &mut Bits::new(0)).is_ok()
        })
    }

    /// `zend_inference_propagate_range` of `var`, which the trace's op
    /// `at` defines, over what `tssa.info` holds of its operands.
    pub fn propagate_range(
        &self,
        tssa: &mut Ssa<T::Class>,
        positions: &[Position],
        at: usize,
        var: SsaVar,
    ) -> Option<Range> {
        self.infer(tssa, positions, |cx| cx.propagate_range(at, var))
    }

    /// `zend_may_throw_ex` at the trace's op `at`, its operands of `t1`
    /// and `t2`, over what `tssa.info` holds of the rest.
    pub fn may_throw(
        &self,
        tssa: &mut Ssa<T::Class>,
        positions: &[Position],
        at: usize,
        (t1, t2): (Types, Types),
    ) -> bool {
        self.infer(tssa, positions, |cx| cx.may_throw_ex(at, t1, t2))
    }
}
