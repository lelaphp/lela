//! The loads a body makes for a word no path reads ([`unread`]).

use cranelift_codegen::entity::EntitySet;
use cranelift_codegen::ir::{BlockArg, Function, Inst, InstBuilder, Value};

/// Each of `loads` whose value no instruction that stays reads, directly
/// or through the block parameters it is passed on as, made a constant.
///
/// Where a leave in place stands the frame, it loads the call under way
/// in the caller, the caller's cache and the stack's end for the
/// [`Stood`](super::super::Stood) the body holds in variables, which a
/// path reads only where a kernel, an exit, a link or an escape needs it;
/// the next push, call or leave in place stands it again, and php's
/// `zend_jit_leave_func` loads none of them. Cranelift keeps every load
/// to its lowering, read or not, since a load may trap; `loads` are
/// trusted ones, which do not, so one nothing reads is dropped unseen.
/// An instruction stays where it calls, branches, ends a block, stores,
/// may trap, has another effect or loads and is none of `loads`, or where
/// a value of its stays; a value stays where an instruction that stays
/// reads it, or passes it to a block parameter that stays.
pub(super) fn unread(func: &mut Function, loads: &[Inst]) {
    if loads.is_empty() {
        return;
    }
    let mut live = EntitySet::<Value>::new();
    let mut grew = true;
    while grew {
        grew = false;
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                if !stays(func, inst, loads, &live) {
                    continue;
                }
                let dfg = &func.dfg;
                for arg in dfg.inst_args(inst) {
                    grew |= live.insert(dfg.resolve_aliases(*arg));
                }
                let insts = &dfg.insts[inst];
                for call in insts.branch_destination(&dfg.jump_tables, &dfg.exception_tables) {
                    let params = dfg.block_params(call.block(&dfg.value_lists));
                    for (param, arg) in params.iter().zip(call.args(&dfg.value_lists)) {
                        if let BlockArg::Value(arg) = arg
                            && live.contains(*param)
                        {
                            grew |= live.insert(dfg.resolve_aliases(arg));
                        }
                    }
                }
            }
        }
    }
    for &load in loads {
        let Some(&value) = func.dfg.inst_results(load).first() else {
            continue;
        };
        if !live.contains(value) {
            let ty = func.dfg.value_type(value);
            func.replace(load).iconst(ty, 0);
        }
    }
}

/// Whether `inst` stays whatever reads its values ([`unread`]).
fn stays(func: &Function, inst: Inst, loads: &[Inst], live: &EntitySet<Value>) -> bool {
    let op = func.dfg.insts[inst].opcode();
    let effect = op.is_call()
        || op.is_branch()
        || op.is_terminator()
        || op.can_trap()
        || op.other_side_effects()
        || op.can_store()
        || op.can_load();
    (effect && !loads.contains(&inst))
        || (func.dfg.inst_results(inst))
            .iter()
            .any(|value| live.contains(*value))
}
