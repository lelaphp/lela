//! The live-range table, `zend_calc_live_ranges` at `pass_two`
//! (`Z/Zend/zend_opcode.c:939`, called at `:1251`): the oplines over
//! which a temporary holds a value the unwinder would have to free were
//! a throw to leave them (design §5.7). php walks the oplines from the
//! last, keeps the opline of each temporary's last use, and emits a range
//! for a definition whose use is not the next opline; `emit_live_range`
//! (`:748`) reads the kind off the defining opcode, and the table is
//! left sorted by start as `swap_live_range` leaves it.
//!
//! What is copied, opcode for opcode: `is_fake_def` and `keeps_op1_alive`
//! (`:887`, `:893`); a boolean, a class or a `FAST_CALL` emitting no
//! range; `BEGIN_SILENCE` a `silence` and `FE_RESET_*` a `loop` from the
//! opline after theirs; `ROPE_INIT` a `rope` from its own opline;
//! `NEW` a `new` up to and including its constructor's `DO_FCALL`, with
//! a `tmp/var` after it where that is not the use itself; `COPY_TMP`'s
//! range split at the `FREE`s of its null branch; `SEPARATE` redeclaring
//! nothing; `FE_FETCH_*`'s `op2` a definition; and a `FREE` or `FE_FREE`
//! flagged `ZEND_FREE_ON_RETURN` splitting the range before the return it
//! precedes and recording the distance to the loop's end in its `op2`.
//! `needs_live_range` is null at `pass_two`, so no range is left out.

use php_unit::{LiveKind, LiveRange, Opcode, OperandKind, Opline, OplineIdx};

/// `ZEND_FREE_ON_RETURN`, the bit `zend_compile.h:1104` sets on a `FREE`
/// or `FE_FREE` emitted for an early return out of a loop.
const FREE_ON_RETURN: u32 = 1;

/// Whether an operand kind is one of `IS_TMP_VAR | IS_VAR`.
fn is_temporary(kind: OperandKind) -> bool {
    matches!(kind, OperandKind::Tmp | OperandKind::Var)
}

/// `is_fake_def`: an opcode that modifies what its `out` holds and does not
/// create it.
fn is_fake_def(op: &Opline) -> bool {
    matches!(
        op.opcode,
        Opcode::RopeAdd | Opcode::AddArrayElement | Opcode::AddArrayUnpack
    )
}

/// `keeps_op1_alive`: an opcode that does not consume its `op1`, which
/// something later frees.
fn keeps_op1_alive(op: &Opline) -> bool {
    matches!(
        op.opcode,
        Opcode::Case
            | Opcode::CaseStrict
            | Opcode::SwitchLong
            | Opcode::SwitchString
            | Opcode::Match
            | Opcode::MatchError
            | Opcode::FetchListR
            | Opcode::FetchListW
            | Opcode::CopyTmp
            | Opcode::ExtStmt
    )
}

/// The table as it grows, `emit_live_range_raw` appending, and the walk's
/// `last_use` per temporary.
struct Ranges {
    table: Vec<LiveRange>,
    last_use: Vec<Option<u32>>,
}

impl Ranges {
    /// `emit_live_range_raw`: one row, `start < end` as php asserts and
    /// nothing written where it is not.
    fn raw(&mut self, var: u32, kind: LiveKind, start: u32, end: u32) {
        if start < end {
            self.table.push(LiveRange {
                var,
                kind,
                start: OplineIdx(start),
                end: OplineIdx(end),
            });
        }
    }

    fn last_use(&self, var: u32) -> Option<u32> {
        self.last_use.get(var as usize).copied().flatten()
    }

    fn set_last_use(&mut self, var: u32, at: Option<u32>) {
        if let Some(slot) = self.last_use.get_mut(var as usize) {
            *slot = at;
        }
    }

    /// `emit_live_range`: the range of `var` defined at `start` and last
    /// used at `end`, with the kind the defining opcode gives it.
    fn emit(&mut self, oplines: &[Opline], var: u32, mut start: u32, end: u32) {
        let Some(def) = oplines.get(start as usize) else {
            return;
        };
        let kind = match def.opcode {
            // The first three are never the first definition; a boolean,
            // a class and a FAST_CALL need no freeing.
            Opcode::AddArrayElement
            | Opcode::AddArrayUnpack
            | Opcode::RopeAdd
            | Opcode::JmpzEx
            | Opcode::JmpnzEx
            | Opcode::Bool
            | Opcode::BoolNot
            | Opcode::FetchClass
            | Opcode::DeclareAnonClass
            | Opcode::FastCall => return,
            Opcode::BeginSilence => {
                start += 1;
                LiveKind::Silence
            }
            // A rope's range includes the opline that starts it.
            Opcode::RopeInit => LiveKind::Rope,
            Opcode::FeResetR | Opcode::FeResetRw => {
                start += 1;
                LiveKind::Loop
            }
            // The object is initialised only after the constructor's
            // DO_FCALL: a `new` range up to it, and a `tmp/var` after.
            Opcode::New => {
                let constructed = constructor_call(oplines, start, end);
                self.raw(var, LiveKind::New, start + 1, constructed + 1);
                if constructed + 1 == end {
                    return;
                }
                start = constructed + 1;
                LiveKind::TmpVar
            }
            Opcode::CopyTmp => {
                self.copy_tmp(oplines, var, start, end);
                return;
            }
            _ => {
                start += 1;
                LiveKind::TmpVar
            }
        };
        self.raw(var, kind, start, end);
    }

    /// `COPY_TMP`'s split range: one from the definition to the use in
    /// the null branch, and one from the first of the null branch's
    /// `FREE`s to the `FREE` that is the use.
    fn copy_tmp(&mut self, oplines: &[Opline], var: u32, start: u32, end: u32) {
        let Some(use_op) = oplines.get(end as usize) else {
            return;
        };
        if use_op.opcode != Opcode::Free {
            // One branch of the coalesce was optimised away.
            self.raw(var, LiveKind::TmpVar, start + 1, end);
            return;
        }
        let mut block_start = end;
        while block_start > 0
            && oplines
                .get(block_start as usize - 1)
                .is_some_and(|op| op.opcode == Opcode::Free)
        {
            block_start -= 1;
        }
        self.raw(var, LiveKind::TmpVar, block_start, end);
        let mut at = end;
        loop {
            if at == 0 {
                return;
            }
            at -= 1;
            let Some(op) = oplines.get(at as usize) else {
                return;
            };
            if op.opcode == Opcode::CopyTmp && op.out == var {
                // The use was optimised away and this is the def.
                self.raw(var, LiveKind::TmpVar, start + 1, end);
                return;
            }
            if (is_temporary(op.op1_type) && op.op1 == var)
                || (is_temporary(op.op2_type) && op.op2 == var)
            {
                break;
            }
        }
        self.raw(var, LiveKind::TmpVar, start + 1, at);
    }

    /// A definition of `var` at `opnum`: the range to its last use where
    /// that use is not the next opline, and the use forgotten. A
    /// definition without a use is a value nothing reads, or the earlier
    /// of two defining opcodes, and emits nothing.
    fn define(&mut self, oplines: &[Opline], var: u32, opnum: u32) {
        if let Some(last) = self.last_use(var) {
            if opnum + 1 != last {
                self.emit(oplines, var, opnum, last);
            }
            self.set_last_use(var, None);
        }
    }
}

/// The opline of the `DO_FCALL` that runs the constructor of the `NEW`
/// at `def`, skipping the calls nested in its arguments; `use_at - 1`
/// where none stands before the use.
fn constructor_call(oplines: &[Opline], def: u32, use_at: u32) -> u32 {
    let mut at = def;
    let mut level = 0u32;
    while at + 1 < use_at {
        at += 1;
        match oplines.get(at as usize).map(|op| op.opcode) {
            Some(
                Opcode::InitFcall
                | Opcode::InitFcallByName
                | Opcode::InitNsFcallByName
                | Opcode::InitDynamicCall
                | Opcode::InitUserCall
                | Opcode::InitMethodCall
                | Opcode::InitStaticMethodCall
                | Opcode::InitParentPropertyHookCall
                | Opcode::New,
            ) => level += 1,
            Some(Opcode::DoFcall | Opcode::DoFcallByName | Opcode::DoIcall | Opcode::DoUcall) => {
                if level == 0 {
                    break;
                }
                level -= 1;
            }
            _ => {}
        }
    }
    at
}

/// `zend_calc_live_ranges` over one op array's oplines: the table,
/// sorted by start. `var` on each row is the temporary's own number, as
/// the operands hold it. A `FREE` flagged `ZEND_FREE_ON_RETURN` has the
/// distance to the loop's end written into its `op2`, which is why the
/// oplines are taken mutably.
pub(crate) fn live_ranges(oplines: &mut [Opline], num_tmps: u32) -> Vec<LiveRange> {
    let mut ranges = Ranges {
        table: Vec::new(),
        last_use: vec![None; num_tmps as usize],
    };
    let mut opnum = u32::try_from(oplines.len()).unwrap_or(u32::MAX);
    while opnum > 0 {
        opnum -= 1;
        let Some(op) = oplines.get(opnum as usize).copied() else {
            break;
        };
        // SEPARATE redeclares its op1; for live ranges its declaration
        // is nothing, and it must not end COPY_TMP's range.
        if op.opcode == Opcode::Separate {
            continue;
        }
        if is_temporary(op.out_type) && !is_fake_def(&op) {
            ranges.define(oplines, op.out, opnum);
        }
        if is_temporary(op.op1_type) {
            let var = op.op1;
            if ranges.last_use(var).is_none() {
                if !keeps_op1_alive(&op) {
                    // OP_DATA is part of the opline before it.
                    let at = opnum - u32::from(op.opcode == Opcode::OpData);
                    ranges.set_last_use(var, Some(at));
                }
            } else if matches!(op.opcode, Opcode::Free | Opcode::FeFree)
                && op.extended_value & FREE_ON_RETURN != 0
            {
                let Some(last) = ranges.last_use(var) else {
                    continue;
                };
                let at = |offset: u32| oplines.get((opnum + offset) as usize);
                let mut jump = 1;
                while at(jump).is_some_and(|next| {
                    matches!(next.opcode, Opcode::Free | Opcode::FeFree)
                        && next.extended_value & FREE_ON_RETURN != 0
                }) {
                    jump += 1;
                }
                // The loop's frees stand right before the jump or the
                // return, except that VERIFY_RETURN_TYPE may come first.
                if at(jump).is_some_and(|next| next.opcode == Opcode::VerifyReturnType) {
                    jump += 1;
                }
                // A FREE flagged for the early return frees the loop
                // variable there, so the range is split around it.
                if opnum + jump + 1 != last {
                    let kind = if op.opcode == Opcode::FeFree {
                        LiveKind::Loop
                    } else {
                        LiveKind::TmpVar
                    };
                    ranges.raw(var, kind, opnum + jump + 1, last);
                }
                ranges.set_last_use(var, Some(opnum));
                let mut distance = last - opnum;
                if let Some(last_op) = oplines.get(last as usize)
                    && last_op.extended_value & FREE_ON_RETURN != 0
                {
                    distance += last_op.op2;
                }
                if let Some(this) = oplines.get_mut(opnum as usize) {
                    this.op2 = distance;
                }
            }
        }
        if is_temporary(op.op2_type) {
            let var = op.op2;
            if matches!(op.opcode, Opcode::FeFetchR | Opcode::FeFetchRw) {
                // FE_FETCH's op2 is a definition, not a use.
                ranges.define(oplines, var, opnum);
            } else if ranges.last_use(var).is_none() {
                ranges.set_last_use(var, Some(opnum));
            }
        }
    }
    // The walk left the table in reverse; php reverts it, then, from the
    // first range whose start is after the next one's, sorts the rest
    // with `zend_sort` by start alone. Its networks for a handful of
    // ranges are not stable, so ranges of equal starts stand in the
    // order php's sort leaves them, which a stable sort would not: the
    // `rope` of a constructor's argument before the `new` that holds the
    // object, where a third range after them is what set the sort off.
    ranges.table.reverse();
    let unsorted = ranges
        .table
        .windows(2)
        .position(|pair| matches!(pair, [a, b] if a.start > b.start));
    if let Some(first) = unsorted
        && let Some(rest) = ranges.table.get_mut(first..)
    {
        php_heap::zend_sort(rest, &mut |a, b| a.start.cmp(&b.start));
    }
    ranges.table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(opcode: Opcode) -> Opline {
        Opline {
            opcode,
            ..Opline::default()
        }
    }

    fn out(opcode: Opcode, kind: OperandKind, var: u32) -> Opline {
        Opline {
            opcode,
            out_type: kind,
            out: var,
            ..Opline::default()
        }
    }

    fn use1(opcode: Opcode, kind: OperandKind, var: u32) -> Opline {
        Opline {
            opcode,
            op1_type: kind,
            op1: var,
            ..Opline::default()
        }
    }

    fn range(var: u32, kind: LiveKind, start: u32, end: u32) -> LiveRange {
        LiveRange {
            var,
            kind,
            start: OplineIdx(start),
            end: OplineIdx(end),
        }
    }

    /// A use on the next opline is trivial and a definition nothing
    /// reads emits nothing: `T0 = ADD; ECHO T0; V1 = DO_ICALL`.
    #[test]
    fn a_trivial_range_and_an_unread_definition_emit_nothing() {
        use OperandKind::{Tmp, Var};
        let mut oplines = [
            out(Opcode::Add, Tmp, 0),
            use1(Opcode::Echo, Tmp, 0),
            out(Opcode::DoIcall, Var, 1),
        ];
        assert!(live_ranges(&mut oplines, 2).is_empty());
    }

    /// `FE_RESET_R` starts a `loop` one opline after itself, ended by the
    /// `FE_FREE`; `BEGIN_SILENCE` a `silence` one after, ended by
    /// `END_SILENCE`; `ROPE_INIT` a `rope` at its own opline, `ROPE_ADD`
    /// no definition and `ROPE_END` the use. The table is sorted by
    /// start, where the walk from the last opline met them reversed.
    #[test]
    fn loop_silence_and_rope_take_their_kinds_from_the_defining_opcode() {
        use OperandKind::{Tmp, Var};
        let mut oplines = [
            out(Opcode::FeResetR, Var, 0),
            out(Opcode::BeginSilence, Tmp, 1),
            op(Opcode::Nop),
            use1(Opcode::EndSilence, Tmp, 1),
            out(Opcode::RopeInit, Tmp, 2),
            out(Opcode::RopeAdd, Tmp, 2),
            out(Opcode::RopeEnd, Tmp, 3),
            use1(Opcode::FeFree, Var, 0),
        ];
        oplines[6].op1_type = Tmp;
        oplines[6].op1 = 2;
        assert_eq!(
            *live_ranges(&mut oplines, 4),
            [
                range(0, LiveKind::Loop, 1, 7),
                range(1, LiveKind::Silence, 2, 3),
                range(2, LiveKind::Rope, 4, 6),
            ]
        );
    }

    /// `NEW`'s `new` range runs to its constructor's `DO_FCALL`, one
    /// nested call deep, and a use past that is a `tmp/var` after it; a
    /// `NEW` whose use is the opline after the `DO_FCALL` has no second
    /// range.
    #[test]
    fn new_is_split_at_the_constructor_s_do_fcall() {
        use OperandKind::Var;
        let mut oplines = [
            out(Opcode::New, Var, 0),
            op(Opcode::InitFcall),
            op(Opcode::DoIcall),
            op(Opcode::DoFcall),
            op(Opcode::Nop),
            use1(Opcode::Echo, Var, 0),
            out(Opcode::New, Var, 1),
            op(Opcode::DoFcall),
            use1(Opcode::Free, Var, 1),
        ];
        assert_eq!(
            *live_ranges(&mut oplines, 2),
            [
                range(0, LiveKind::New, 1, 4),
                range(0, LiveKind::TmpVar, 4, 5),
                range(1, LiveKind::New, 7, 8),
            ]
        );
    }

    /// `COPY_TMP`'s split: from the definition to the last use before the
    /// null branch's `FREE`s, and from the first of those `FREE`s to the
    /// one that uses it.
    #[test]
    fn copy_tmp_s_range_is_split_at_the_null_branch_s_frees() {
        use OperandKind::Tmp;
        let mut oplines = [
            out(Opcode::CopyTmp, Tmp, 0),
            op(Opcode::Nop),
            use1(Opcode::JmpNull, Tmp, 0),
            op(Opcode::Nop),
            use1(Opcode::Free, Tmp, 1),
            use1(Opcode::Free, Tmp, 0),
        ];
        oplines[0].op1_type = Tmp;
        oplines[0].op1 = 5;
        assert_eq!(
            *live_ranges(&mut oplines, 6),
            [
                range(0, LiveKind::TmpVar, 1, 2),
                range(0, LiveKind::TmpVar, 4, 5),
            ]
        );
    }

    /// A `FE_FREE` flagged `ZEND_FREE_ON_RETURN` before a `RETURN` splits
    /// the loop's range around the return and records the distance to
    /// the loop's own `FE_FREE` in its `op2`.
    #[test]
    fn a_free_on_return_splits_the_range_and_records_the_distance() {
        use OperandKind::Var;
        let mut oplines = [
            out(Opcode::FeResetR, Var, 0),
            op(Opcode::Nop),
            Opline {
                extended_value: FREE_ON_RETURN,
                ..use1(Opcode::FeFree, Var, 0)
            },
            op(Opcode::Return),
            op(Opcode::Nop),
            use1(Opcode::FeFree, Var, 0),
        ];
        assert_eq!(
            *live_ranges(&mut oplines, 1),
            [
                range(0, LiveKind::Loop, 1, 2),
                range(0, LiveKind::Loop, 4, 5),
            ]
        );
        assert_eq!(
            oplines[2].op2, 3,
            "the distance from the early FREE to the loop's"
        );
    }
}
