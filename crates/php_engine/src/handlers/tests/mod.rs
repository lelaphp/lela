/// The edge a handler called outside the loop served, as
/// [`crate::tier::step`] hands it out: the position the loop would
/// have held written back to the frame, which the handler was
/// entered at as the dispatch enters it.
macro_rules! served {
    ($req:ident, $frame:expr, $at:ident => $call:expr) => {{
        let $at = $req.vm().opline($frame);
        let edge = $call;
        edge.map(|next| crate::tier::settled(&mut $req, $frame, $at, next))
    }};
}
use std::sync::Arc;

use php_heap::Intern as _;
use php_unit::{Opcode, OperandKind, PublishedUnit, Unit};

use super::*;
use crate::api::request::Leaving;
use crate::api::values::ClassId;
use crate::engine::{Engine, RequestDescription};
use crate::probe::{Asked, Probe};
use crate::tier::Nop;

/// A unit whose main op array is one `JMP` to `target`, its
/// safepoint number `number`.
fn jumping(target: u32, number: u32) -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![
                Opline::default(),
                Opline {
                    opcode: Opcode::Jmp,
                    op1: target,
                    out: number,
                    ..Opline::default()
                },
            ],
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// Design §5.7, §5.8: a `JMP` to an earlier opline is a back-edge and
/// raises the loop safepoint with the number the compiler wrote; one
/// to a later opline raises nothing. Both leave the frame at the
/// target, which is the opline a tier is asked with.
#[test]
fn a_backward_jump_raises_the_loop_safepoint_and_a_forward_one_does_not() {
    let engine = Engine::cli_builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let unit = jumping(0, 1);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let back = code.oplines.get(1).copied().expect("opline 1");
    req.vm_mut().stand(frame, 1);

    assert_eq!(
        served!(req, frame, at => jmp::<Probe>(&mut req, frame, code, &back)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().opline(frame), 0);
    assert_eq!(
        req.state::<Asked>().sites,
        vec![(Site::Loop(SafepointIdx(1)), frame)]
    );

    let forward = Opline {
        opcode: Opcode::Jmp,
        op1: 1,
        ..Opline::default()
    };
    assert_eq!(
        served!(req, frame, at => jmp::<Probe>(&mut req, frame, code, &forward)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().opline(frame), 1);
    assert_eq!(req.state::<Asked>().sites.len(), 1);
}

/// Design §5.8: the interrupt check at the back-edge, before the
/// site, so a request whose time is up stops within one iteration
/// and the tier is not asked.
#[test]
fn the_interrupt_is_tested_at_a_back_edge() {
    let engine = Engine::cli_builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let unit = jumping(0, 1);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let back = code.oplines.get(1).copied().expect("opline 1");
    req.vm_mut().stand(frame, 1);
    req.limits().interrupt().elapsed(1);

    assert_eq!(
        served!(req, frame, at => jmp::<Probe>(&mut req, frame, code, &back)),
        Err(Fatal::error("Maximum execution time of 1 second exceeded"))
    );
    assert_eq!(req.state::<Asked>().sites, vec![]);
}

/// A unit whose main op array is `NOP` then a `JMPNZ` over the
/// constant at slot zero, aimed at the `NOP`, with safepoint number
/// one: the shape of `for (;;)` after a body of one opline.
fn conditional(literal: php_heap::Zval) -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![
                Opline::default(),
                Opline {
                    opcode: Opcode::Jmpnz,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    op2: 0,
                    out: 1,
                    ..Opline::default()
                },
            ],
            literals: vec![literal],
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// Design §5.7, §5.8: a `JMPNZ` taken to an earlier opline is the
/// back-edge and raises the loop safepoint as `JMP` does; one not
/// taken goes on to the next opline and raises nothing.
#[test]
fn a_conditional_jump_is_the_back_edge_only_when_it_is_taken() {
    let engine = Engine::cli_builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let taken = conditional(php_heap::Zval::bool(true));
    let frame = req.pushed(PublishedUnit::clone(&taken));
    let code = crate::exec::code_of(&taken);
    let op = code.oplines.get(1).copied().expect("opline 1");
    req.vm_mut().stand(frame, 1);

    assert_eq!(
        served!(req, frame, at => jmpnz::<Probe, crate::operand::Const>(&mut req, frame, code, &op, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().opline(frame), 0);
    assert_eq!(
        req.state::<Asked>().sites,
        vec![(Site::Loop(SafepointIdx(1)), frame)]
    );

    let not_taken = conditional(php_heap::Zval::long(0));
    let frame = req.pushed(PublishedUnit::clone(&not_taken));
    let code = crate::exec::code_of(&not_taken);
    req.vm_mut().stand(frame, 1);
    assert_eq!(
        served!(req, frame, at => jmpnz::<Probe, crate::operand::Const>(&mut req, frame, code, &op, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().opline(frame), 2, "on to the opline after it");
    assert_eq!(
        req.state::<Asked>().sites.len(),
        1,
        "nothing more was asked"
    );
}

/// A unit whose main op array is `NOP`, `IS_SMALLER` over the
/// constants `left` and one into temporary zero, and a `JMPNZ` over
/// that temporary aimed at the `NOP` with safepoint number seven:
/// `do {} while ($i < 1)` over a constant.
fn compared_loop(left: i64) -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![
                Opline::default(),
                Opline {
                    opcode: Opcode::IsSmaller,
                    op1_type: OperandKind::Const,
                    op2_type: OperandKind::Const,
                    op2: 1,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    ..Opline::default()
                },
                Opline {
                    opcode: Opcode::Jmpnz,
                    op1_type: OperandKind::Tmp,
                    op1: 0,
                    op2: 0,
                    out: 7,
                    ..Opline::default()
                },
                Opline::default(),
            ],
            literals: vec![php_heap::Zval::long(left), php_heap::Zval::long(1)],
            num_tmps: 1,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// Design §5.8: a comparison with a `JMPNZ` over its temporary after
/// it is one handler over both oplines. Taken backward, the fused
/// handler raises one loop safepoint with the branch opline's
/// number and leaves the frame at the target; not taken, one `step`
/// lands past the jump, raising nothing.
#[test]
fn a_comparison_branches_over_the_jump_after_it_as_one_step() {
    let engine = Engine::builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let taken = compared_loop(0);
    let frame = req.pushed(PublishedUnit::clone(&taken));
    let code = crate::exec::code_of(&taken);
    let op = code.oplines.get(1).copied().expect("opline 1");
    req.vm_mut().stand(frame, 1);

    assert_eq!(
        served!(req, frame, at => is_smaller::<Probe, crate::operand::Const, crate::operand::Const, branch::OnJmpnz>(
            &mut req, frame, code, &op, at
        )),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().opline(frame), 0);
    assert_eq!(
        req.state::<Asked>().sites,
        vec![(Site::Loop(SafepointIdx(7)), frame)]
    );

    let not_taken = compared_loop(1);
    let frame = req.pushed(PublishedUnit::clone(&not_taken));
    req.vm_mut().stand(frame, 1);
    assert_eq!(crate::tier::step(&mut req, frame), Ok(Stepped::Same(frame)));
    assert_eq!(req.vm().opline(frame), 3, "past the jump in one step");
    assert_eq!(req.state::<Asked>().sites.len(), 1);
}

/// A unit whose main op array is one `RETURN null`, its safepoint
/// number `number`, as the compiler writes it into `result`.
fn returning(number: u32) -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::Return,
                op1_type: OperandKind::Const,
                op1: 0,
                out: number,
                ..Opline::default()
            }],
            literals: vec![php_heap::Zval::null()],
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// Design §4.3 seam 8: `RETURN` raises the return safepoint, with the
/// number the compiler wrote, for a frame that is not the entry frame
/// of this serving, and pops it to its caller.
#[test]
fn a_return_in_no_entry_frame_raises_the_return_safepoint_and_pops() {
    let engine = Engine::cli_builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let unit = returning(1);
    let outer = req.pushed(PublishedUnit::clone(&unit));
    let guard = req.stack_mut().push_entry(outer);
    let inner = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let op = code.oplines.first().copied().expect("opline 0");

    assert_eq!(
        served!(req, inner, at => r#return::<Probe, crate::operand::Const>(&mut req, inner, code, &op, at)),
        Ok(Stepped::Popped(outer))
    );
    assert_eq!(
        req.state::<Asked>().sites,
        vec![(Site::Return(SafepointIdx(1)), inner)]
    );

    // The entry frame's own return raises nothing and ends the loop.
    assert_eq!(
        served!(req, outer, at => r#return::<Probe, crate::operand::Const>(&mut req, outer, code, &op, at)),
        Ok(Stepped::Done)
    );
    assert_eq!(req.state::<Asked>().sites.len(), 1);
    req.stack_mut().pop_entry(guard);
}

/// A unit whose main op array is `INIT_FCALL_BY_NAME` over the pair
/// `StrLen`, `strlen`, a `SEND_VAL_EX` of the constant `"abc"` and a
/// `DO_FCALL_BY_NAME` into `V0`: the shape php's compiler emits for
/// `StrLen("abc")` where its function table has not the name at
/// compile time and the program declares it before the call runs.
fn by_name(strings: &mut php_heap::Strings) -> PublishedUnit {
    let written = interned(strings, b"StrLen");
    let lowered = interned(strings, b"strlen");
    let argument = interned(strings, b"abc");
    Unit {
        main: OpArray {
            oplines: vec![
                Opline {
                    opcode: Opcode::InitFcallByName,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out: 0,
                    extended_value: 1,
                    ..Opline::default()
                },
                Opline {
                    opcode: Opcode::SendValEx,
                    op1_type: OperandKind::Const,
                    op1: 2,
                    op2: 1,
                    out: 0,
                    ..Opline::default()
                },
                Opline {
                    opcode: Opcode::DoFcallByName,
                    out_type: OperandKind::Var,
                    out: 0,
                    ..Opline::default()
                },
            ],
            literals: vec![written, lowered, argument],
            num_tmps: 1,
            cache_size: 8,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// Interned `bytes` as a constant.
fn interned(strings: &mut php_heap::Strings, bytes: &[u8]) -> php_heap::Zval {
    php_heap::Zval::string(strings.intern(bytes).expect("intern a constant"))
}

/// `ZEND_INIT_FCALL_BY_NAME` resolves the pair's lowered name into
/// the frame `INIT_FCALL` builds, `SEND_VAL_EX` places the argument
/// on it as `SEND_VAL` does and `DO_FCALL_BY_NAME` serves the native
/// as `DO_ICALL` does, into the `VAR` the opline names. The
/// compiler is handed the bundle's whole table, so no program
/// reaches these three with a name the engine resolves until a
/// program can grow the table; this unit is that program.
#[test]
fn a_call_by_name_resolves_the_lowered_constant_and_serves_the_native() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = by_name(&mut strings);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let init = code.oplines.first().copied().expect("opline 0");
    let send = code.oplines.get(1).copied().expect("opline 1");
    let call = code.oplines.get(2).copied().expect("opline 2");

    assert_eq!(
        served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
        Ok(Stepped::Same(frame))
    );
    assert!(!req.is_pending(), "the lowered constant is the table's key");
    assert_eq!(
        served!(req, frame, at => send_val_ex::<Nop>(&mut req, frame, code, &send, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(
        served!(req, frame, at => do_fcall_by_name::<Nop>(&mut req, frame, code, &call)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.slot(frame, 0).as_int(), Some(3));
    assert_eq!(req.vm().opline(frame), 3);
}

/// `ZEND_INIT_FCALL_BY_NAME` over two passes of one opline: the
/// first finds its cache slot unprimed, looks the table up once and
/// writes the callee into the slot, `CACHE_PTR(opline->result.num,
/// fbc)`; the second reads the callee off the slot and looks
/// nothing up.
#[test]
fn a_call_by_name_looks_the_table_up_once_and_reads_its_slot_after() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = by_name(&mut strings);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let init = code.oplines.first().copied().expect("opline 0");
    assert_eq!(req.cached(frame, CacheSlot(init.out)), Cached::Unprimed);

    assert_eq!(
        served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().lookups, 1, "the first pass looks the table up");
    let strlen = req.function_named(Bare::resolved(b"strlen"));
    assert!(strlen.is_some());
    assert_eq!(
        req.cached(frame, CacheSlot(init.out)),
        Cached::Function(strlen.unwrap_or(Function::PASS)),
        "and primes the slot with the callee"
    );

    assert_eq!(
        served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().lookups, 1, "the second pass reads the slot");
    assert_eq!(
        popped_callee(&mut req),
        strlen,
        "and pushes the same callee"
    );
    assert_eq!(popped_callee(&mut req), strlen);
}

/// A unit declaring `f`, whose body is the `INIT_FCALL_BY_NAME` of
/// [`by_name`] alone, and a main op array with a cache slot of its
/// own, not yet shared.
fn declaring_f(strings: &mut php_heap::Strings) -> Unit {
    let by_name = by_name(strings);
    let mut body = by_name.main.clone();
    body.oplines = body.oplines.iter().take(1).copied().collect();
    let decl = php_unit::decl::native_function(b"f");
    Unit {
        main: OpArray {
            cache_size: 8,
            ..OpArray::default()
        },
        functions: vec![(decl, body)],
        ..Unit::default()
    }
}

/// The runtime cache is the op array's for the request and not the
/// frame's (`ZEND_MAP_PTR(op_array->run_time_cache)`, laid down by
/// `init_func_run_time_cache` on the first activation): a function
/// called twice finds on its second call the slot its first call
/// primed, so the `INIT_FCALL_BY_NAME` in its body looks the table
/// up once across the two calls, and the main op array's own slot
/// stands at another entry and is not what it primed.
#[test]
fn a_function_called_twice_reads_on_its_second_call_what_its_first_primed() {
    let engine = Engine::cli_builder().build();
    let req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let mut unit = declaring_f(&mut strings);
    crate::engine::assign_map_ptrs(&mut unit, &engine.shared.map_ptr_last);
    called_twice(req, &unit.published(None));
}

/// The same over a unit compiled for the request alone, whose op
/// arrays take caches of the request's own and none of the engine's
/// map-pointer offsets, php's heap caches (`ZEND_ACC_HEAP_RT_CACHE`):
/// the function's second call still reads what its first primed, and
/// the engine's count has not moved (#528).
#[test]
fn a_request_only_unit_s_function_called_twice_reads_what_its_first_call_primed() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let count = || {
        engine
            .shared
            .map_ptr_last
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let before = count();
    let unit = req.take_unit(declaring_f(&mut strings));
    called_twice(req, &unit);
    assert_eq!(count(), before, "the engine's count is where it was");
}

/// `f` of `unit` called twice from its main op array: one lookup of
/// the table across both calls, each call's slot primed with the
/// callee, and the main op array's own slot not what they primed.
fn called_twice(mut req: Request, unit: &PublishedUnit) {
    let main = req.pushed(PublishedUnit::clone(unit));
    req.declare_functions(unit);
    let f = req
        .function_named(Bare::resolved(b"f"))
        .expect("the unit declares f");
    let code = crate::exec::body_of(unit, exec::Body::Function(0))
        .cloned()
        .expect("f's body");
    let init = code.oplines.first().copied().expect("opline 0");
    let strlen = req.function_named(Bare::resolved(b"strlen"));

    for call in 1..=2 {
        let mut header = exec::Header::call(f, 0);
        header.info.set_serving();
        let frame = req
            .push_call(header)
            .and_then(|call| {
                let shape = req.call_shape(call).expect("the call's shape");
                req.activate_user(call, shape, None, crate::exec::Caller::Opline(main))
            })
            .map_or(main, |activated| activated.frame);
        assert_eq!(
            served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, &code, &init, at)),
            Ok(Stepped::Same(frame)),
            "call {call}"
        );
        assert_eq!(
            req.vm().lookups,
            1,
            "the first call looks the table up and the second reads the slot (call {call})"
        );
        assert_eq!(
            req.cached(frame, CacheSlot(init.out)),
            Cached::Function(strlen.unwrap_or(Function::PASS)),
            "the slot is primed with the callee (call {call})"
        );
        assert_eq!(popped_callee(&mut req), strlen, "call {call}");
        req.pop_frame();
    }
    assert_eq!(
        req.cached(main, CacheSlot(0)),
        Cached::Unprimed,
        "the main op array's cache is its own"
    );
}

/// The call last pushed off the stack, its callee, the name it held
/// given up.
fn popped_callee(req: &mut Request) -> Option<Function> {
    let call = req.building_call()?;
    let callee = req.vm().at(call)?.function;
    req.pop_call();
    callee
}

/// `ZEND_INIT_NS_FCALL_BY_NAME` over `Foo\StrLen`, `foo\strlen`,
/// `strlen`: the first pass makes the handler's two lookups, the
/// namespaced name that misses and the global one that hits, and
/// primes the slot; the second pass makes neither.
#[test]
fn a_namespaced_call_by_name_looks_the_table_up_twice_once() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::InitNsFcallByName,
                op2_type: OperandKind::Const,
                op2: 0,
                out: 8,
                extended_value: 0,
                ..Opline::default()
            }],
            literals: vec![
                interned(&mut strings, b"Foo\\StrLen"),
                interned(&mut strings, b"foo\\strlen"),
                interned(&mut strings, b"strlen"),
            ],
            cache_size: 16,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let init = code.oplines.first().copied().expect("opline 0");
    let strlen = req.function_named(Bare::resolved(b"strlen"));

    assert_eq!(
        served!(req, frame, at => init_ns_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().lookups, 2, "foo\\strlen misses, strlen hits");
    assert_eq!(
        req.cached(frame, CacheSlot(8)),
        Cached::Function(strlen.unwrap_or(Function::PASS))
    );
    assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

    assert_eq!(
        served!(req, frame, at => init_ns_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.vm().lookups, 2, "the second pass looks nothing up");
    assert_eq!(popped_callee(&mut req), strlen);
    assert_eq!(popped_callee(&mut req), strlen);
}

/// A miss primes nothing: php's `zend_undefined_function_helper` is
/// dispatched to before `CACHE_PTR`, so a second pass over a name
/// nothing declares looks the table up again and throws again.
#[test]
fn a_call_by_name_nothing_declares_primes_no_slot() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::InitFcallByName,
                op2_type: OperandKind::Const,
                op2: 0,
                out: 0,
                ..Opline::default()
            }],
            literals: vec![
                interned(&mut strings, b"NoPe"),
                interned(&mut strings, b"nope"),
            ],
            cache_size: 8,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let init = code.oplines.first().copied().expect("opline 0");
    for pass in 1..=2 {
        assert_eq!(
            served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
            Ok(Stepped::Threw(frame))
        );
        assert_eq!(req.vm().lookups, pass);
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);
        assert_eq!(req.uncaught(), Err(Leaving));
        // Each pass is a serving of its own, whose end catches the
        // bailout the rendering began.
        req.catch_bailout();
    }
}

/// A unit whose main op array is one `CATCH` of the class the pair
/// `written`/`lowered` names, the last clause of its `try`
/// (`ZEND_LAST_CATCH`), naming no variable, its cache slot at byte
/// zero and its jump to the `NOP` after it.
fn catching(strings: &mut php_heap::Strings, written: &[u8], lowered: &[u8]) -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![
                Opline {
                    opcode: Opcode::Catch,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    op2: 1,
                    out_type: OperandKind::Unused,
                    extended_value: LAST_CATCH,
                    ..Opline::default()
                },
                Opline::default(),
            ],
            literals: vec![interned(strings, written), interned(strings, lowered)],
            cache_size: 8,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// `ZEND_CATCH` over two passes of one opline, which is what a
/// `catch` inside a loop is: the first finds its cache slot
/// unprimed, looks the class up once and writes the clause's
/// class into the slot the `extended_value` carries beside
/// `ZEND_LAST_CATCH`, `CACHE_PTR(opline->extended_value &
/// ~ZEND_LAST_CATCH, catch_ce)`; the second reads it back and looks
/// nothing up.
#[test]
fn a_catch_looks_the_class_up_once_and_reads_its_slot_after() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = catching(&mut strings, b"Error", b"error");
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let clause = code.oplines.first().copied().expect("opline 0");
    let error = req
        .class_named(Bare::resolved(b"error"))
        .expect("the engine declares Error");
    assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

    for pass in 1..=2 {
        let thrown = crate::api::raising::throw_error(&mut req, "Error", "e");
        req.leave_pending(&thrown);
        req.vm_mut().stand(frame, 0);
        assert_eq!(
            served!(req, frame, at => catch::<Nop>(&mut req, frame, code, &clause)),
            Ok(Stepped::Same(frame))
        );
        assert!(
            req.pending().is_none(),
            "the clause caught it on pass {pass}"
        );
        assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
        assert!(
            matches!(req.cached(frame, CacheSlot(0)), Cached::Class(class) if class == error),
            "the clause's class is in the slot after pass {pass}"
        );
    }
}

/// A unit whose main op array is one `INIT_STATIC_METHOD_CALL` over
/// the class's constant pair at the head of the table and, where
/// `method` is given, the method's pair after it, with the two cache
/// slots the compiler reserves in `result` at byte zero. `method`
/// left out is php's `CONSTRUCTOR` operand, the `C::__construct()`
/// form, whose `op2` is `UNUSED`.
fn static_method_call(
    strings: &mut php_heap::Strings,
    class: (&[u8], &[u8]),
    method: Option<(&[u8], &[u8])>,
) -> PublishedUnit {
    let (op2_type, op2) = match method {
        Some(_) => (OperandKind::Const, 2),
        None => (OperandKind::Unused, 0),
    };
    let mut literals = vec![interned(strings, class.0), interned(strings, class.1)];
    if let Some((written, lowered)) = method {
        literals.push(interned(strings, written));
        literals.push(interned(strings, lowered));
    }
    Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::InitStaticMethodCall,
                op1_type: OperandKind::Const,
                op1: 0,
                op2_type,
                op2,
                out: 0,
                ..Opline::default()
            }],
            literals,
            cache_size: 16,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// `ZEND_INIT_STATIC_METHOD_CALL` over two constants, twice, which
/// is what a call on a class inside a loop is: the first pass finds its
/// slot unprimed, looks the class up once and writes the class and
/// the callee together, `CACHE_POLYMORPHIC_PTR(opline->result.num,
/// ce, fbc)`; the second reads the pair back and looks nothing up
/// and resolves no method.
#[test]
fn a_static_call_over_two_constants_resolves_its_class_and_callee_once() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = static_method_call(
        &mut strings,
        (b"Closure", b"closure"),
        Some((b"fromCallable", b"fromcallable")),
    );
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let op = code.oplines.first().copied().expect("opline 0");
    let closure = req
        .class_named(Bare::resolved(b"closure"))
        .expect("the engine declares Closure");
    let callee = req
        .symbols()
        .method_named(closure, b"fromcallable")
        .expect("Closure::fromCallable is a static method");
    assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

    for pass in 1..=2 {
        req.vm_mut().stand(frame, 0);
        assert_eq!(
            served!(req, frame, at => init_static_method_call::<Nop>(&mut req, frame, code, &op)),
            Ok(Stepped::Same(frame)),
            "the call frame is pushed on pass {pass}"
        );
        assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
        assert!(
            matches!(
                req.cached(frame, CacheSlot(0)),
                Cached::ClassAndCallee { class, scope: None, function, .. }
                    if class == closure && function == callee
            ),
            "the class and the callee are in the slot after pass {pass}"
        );
    }
}

/// php writes the class alone into the first slot only where `op2`
/// is no constant, `if (OP2_TYPE != IS_CONST) CACHE_PTR(...)`: the
/// `C::__construct()` form, whose `op2` is `UNUSED`, primes the
/// class on its first pass and looks nothing up on the next, though
/// the opline ends in php's `Error` for an instance method called
/// on the class either way.
#[test]
fn a_static_call_naming_no_method_primes_the_class_alone() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = static_method_call(&mut strings, (b"Exception", b"exception"), None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let op = code.oplines.first().copied().expect("opline 0");
    let exception = req
        .class_named(Bare::resolved(b"exception"))
        .expect("the engine declares Exception");

    for pass in 1..=2 {
        req.vm_mut().stand(frame, 0);
        assert_eq!(
            served!(req, frame, at => init_static_method_call::<Nop>(&mut req, frame, code, &op)),
            Ok(Stepped::Threw(frame)),
            "a constructor is an instance method on pass {pass}"
        );
        assert!(req.take_pending_view().is_some(), "it threw on pass {pass}");
        assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
        assert!(
            matches!(req.cached(frame, CacheSlot(0)), Cached::Class(class)
                if class == exception),
            "the class alone is in the slot after pass {pass}"
        );
    }
}

/// A unit whose main op array is one `INIT_METHOD_CALL` over the CV
/// `$o` and the pair `GetMessage`, `getmessage`, with the two cache
/// slots the compiler reserves in `result` at byte zero: `$o->GetMessage()`.
fn method_call(strings: &mut php_heap::Strings) -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::InitMethodCall,
                op1_type: OperandKind::Cv,
                op1: 0,
                op2_type: OperandKind::Const,
                op2: 0,
                out: 0,
                ..Opline::default()
            }],
            literals: vec![
                interned(strings, b"GetMessage"),
                interned(strings, b"getmessage"),
            ],
            vars: vec![php_unit::decl::Name::from("o")],
            num_cvs: 1,
            // `zend_alloc_cache_slots(2)`: the class and the callee.
            cache_size: 16,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// `ZEND_INIT_METHOD_CALL` over a constant name, twice on an
/// `Exception` and once on an `Error`, which is what `$o->m()` in a
/// loop over two classes is: the first pass finds the slot
/// unprimed, looks the method up and writes the receiver's class
/// and the callee together, `CACHE_POLYMORPHIC_PTR(opline->result.num,
/// called_scope, fbc)`; the second reads the pair back and looks
/// nothing up; the third, on another class, looks the method up once
/// more and writes that class's pair over it.
#[test]
fn a_method_call_by_a_constant_name_looks_its_callee_up_once_per_class() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = method_call(&mut strings);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let op = code.oplines.first().copied().expect("opline 0");
    let exception = req.class_named(Bare::resolved(b"exception"));
    let error = req.class_named(Bare::resolved(b"error"));
    assert!(
        exception.is_some() && error.is_some(),
        "the engine declares both"
    );
    let callee = |req: &Request, class: Option<ClassId>| {
        class.and_then(|class| req.symbols().method_named(class, b"getmessage"))
    };
    let (on_exception, on_error) = (callee(&req, exception), callee(&req, error));
    assert!(
        on_exception.is_some() && on_exception != on_error,
        "each declares its own"
    );
    assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

    for (pass, class, function, lookups) in [
        (1, exception, on_exception, 1),
        (2, exception, on_exception, 1),
        (3, error, on_error, 2),
    ] {
        let made = class.map(|class| req.new_object(class));
        assert!(
            matches!(made, Some(Ok(_))),
            "an object is made on pass {pass}"
        );
        let Some(Ok(object)) = made else { continue };
        req.replace_slot(frame, 0, object.as_value());
        req.vm_mut().stand(frame, 0);
        assert_eq!(
            served!(req, frame, at => init_method_call::<Nop>(&mut req, frame, code, &op)),
            Ok(Stepped::Same(frame)),
            "the call frame is pushed on pass {pass}"
        );
        req.pop_call();
        assert_eq!(req.vm().lookups, lookups, "lookups over {pass} pass(es)");
        assert!(
            matches!(
                req.cached(frame, CacheSlot(0)),
                Cached::ClassAndCallee { class: held, scope: None, function: callee, .. }
                    if Some(held) == class && Some(callee) == function
            ),
            "the receiver's class and its callee are in the slot after pass {pass}"
        );
    }
}

/// A clause naming a class nothing declares primes nothing, as
/// php's caches the null `zend_lookup_class_ex` gave it and reads it
/// back as unprimed: the table is looked up on every pass, and the
/// throwable the clause does not match is left pending by the
/// `ZEND_LAST_CATCH` for the unwinder.
#[test]
fn a_catch_of_no_class_primes_no_slot() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = catching(&mut strings, b"NoPe", b"nope");
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let clause = code.oplines.first().copied().expect("opline 0");
    let thrown = crate::api::raising::throw_error(&mut req, "Error", "e");
    req.leave_pending(&thrown);

    for pass in 1..=2 {
        req.vm_mut().stand(frame, 0);
        assert_eq!(
            served!(req, frame, at => catch::<Nop>(&mut req, frame, code, &clause)),
            Ok(Stepped::Threw(frame))
        );
        assert_eq!(req.vm().lookups, pass, "looked up again on pass {pass}");
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);
    }
    assert_eq!(req.uncaught(), Err(Leaving));
}

/// The other three handlers that resolve a class off a constant
/// pair read the slot their opline names, `NEW`'s in `op2` and
/// `INSTANCEOF`'s and `FETCH_CLASS_CONSTANT`'s in `extended_value`:
/// a slot primed with `stdClass` answers all three over a pair that
/// names `NoPe`, which nothing declares, so a handler that looked
/// the table up would throw `Class "NoPe" not found` instead. The
/// class constant is one `stdClass` has not, so what it throws
/// names the class the slot held.
#[test]
fn new_instanceof_and_a_class_constant_read_the_class_off_their_slot() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = Unit {
        main: OpArray {
            oplines: vec![
                Opline {
                    opcode: Opcode::New,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    op2: 0,
                    out_type: OperandKind::Var,
                    out: 0,
                    ..Opline::default()
                },
                Opline {
                    opcode: Opcode::DoFcall,
                    ..Opline::default()
                },
                Opline {
                    opcode: Opcode::Instanceof,
                    op1_type: OperandKind::Var,
                    op1: 0,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out_type: OperandKind::Tmp,
                    out: 1,
                    extended_value: 8,
                    ..Opline::default()
                },
                Opline {
                    opcode: Opcode::FetchClassConstant,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    op2_type: OperandKind::Const,
                    op2: 2,
                    out_type: OperandKind::Tmp,
                    out: 2,
                    extended_value: 16,
                    ..Opline::default()
                },
            ],
            literals: vec![
                interned(&mut strings, b"NoPe"),
                interned(&mut strings, b"nope"),
                interned(&mut strings, b"NOPE"),
            ],
            num_tmps: 3,
            cache_size: 24,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let class = req
        .class_named(Bare::resolved(b"stdclass"))
        .expect("the engine declares stdClass");
    for slot in [0, 8, 16] {
        req.prime(frame, CacheSlot(slot), Cached::Class(class));
    }

    let new_op = code.oplines.first().copied().expect("opline 0");
    assert_eq!(
        served!(req, frame, at => new::<Nop>(&mut req, frame, code, &new_op)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(
        req.slot(frame, 0).as_object().map(|v| v.class()),
        Some(class),
        "NEW took its class off op2's slot"
    );

    let instanceof_op = code.oplines.get(2).copied().expect("opline 2");
    assert_eq!(
        served!(req, frame, at => instanceof::<Nop>(&mut req, frame, code, &instanceof_op)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(
        req.slot(frame, 1).as_bool(),
        Some(true),
        "INSTANCEOF took its class off extended_value's slot"
    );

    let constant_op = code.oplines.get(3).copied().expect("opline 3");
    assert_eq!(
        served!(req, frame, at => fetch_class_constant::<Nop>(&mut req, frame, code, &constant_op)),
        Ok(Stepped::Threw(frame))
    );
    assert_eq!(
        pending_message(&req),
        "Undefined constant stdClass::NOPE",
        "FETCH_CLASS_CONSTANT took its class off extended_value's slot"
    );
    assert_eq!(req.vm().lookups, 0, "no handler looked the table up");
}

/// A unit whose main op array is `JMP_FRAMELESS` over `name`, its
/// jump to opline 2, its cache slot at byte 0, then two `NOP`s.
fn jmp_frameless_over(strings: &mut php_heap::Strings, name: &[u8]) -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![
                Opline {
                    opcode: Opcode::JmpFrameless,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    op2: 2,
                    extended_value: 0,
                    ..Opline::default()
                },
                Opline::default(),
                Opline::default(),
            ],
            literals: vec![interned(strings, name)],
            cache_size: 8,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// `ZEND_JMP_FRAMELESS` over two passes of one opline: the first
/// finds `ZEND_JMP_FL_UNPRIMED`, looks the namespaced name up once
/// and primes `ZEND_JMP_FL_HIT` where the table has it not, so the
/// jump is taken; the second reads `HIT` off the slot and jumps with
/// no lookup.
#[test]
fn a_frameless_jump_primes_a_hit_once_and_jumps_on_it_after() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = jmp_frameless_over(&mut strings, b"foo\\substr");
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let jmp = code.oplines.first().copied().expect("opline 0");
    assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);

    for pass in 1..=2 {
        req.vm_mut().stand(frame, 0);
        assert_eq!(
            served!(req, frame, at => jmp_frameless::<Nop>(&mut req, frame, code, &jmp)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            req.vm().opline(frame),
            2,
            "the jump is taken on pass {pass}"
        );
        assert_eq!(req.vm().lookups, 1, "one lookup over {pass} pass(es)");
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Hit);
    }
}

/// The other answer: a name the table has primes `ZEND_JMP_FL_MISS`
/// on the first pass, and the jump is not taken on either.
#[test]
fn a_frameless_jump_primes_a_miss_once_and_falls_through_on_it_after() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = jmp_frameless_over(&mut strings, b"strlen");
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let jmp = code.oplines.first().copied().expect("opline 0");

    for pass in 1..=2 {
        req.vm_mut().stand(frame, 0);
        assert_eq!(
            served!(req, frame, at => jmp_frameless::<Nop>(&mut req, frame, code, &jmp)),
            Ok(Stepped::Same(frame))
        );
        assert_eq!(
            req.vm().opline(frame),
            1,
            "on to the call by name on pass {pass}"
        );
        assert_eq!(req.vm().lookups, 1);
        assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Miss);
    }
}

/// An extension of one native, `seven()`, with a frameless entry of
/// arity zero, which no native of php's declares: what
/// `FRAMELESS_ICALL_0` needs to be reached at all.
fn with_a_nullary_frameless_native() -> Engine {
    use crate::api::abi::ExtensionBuilder;
    use php_unit::decl::FunctionDecl;
    fn seven(_: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::int(7))
    }
    let decl = FunctionDecl {
        frameless: vec![0],
        ..php_unit::decl::native_function(b"seven")
    };
    let extension = ExtensionBuilder::default()
        .functions(vec![(decl, seven as crate::api::abi::Entry)])
        .build("nullary");
    Engine::cli_builder().extensions(&[extension]).build()
}

/// `ZEND_FRAMELESS_ICALL_0` serves the native at the table's row
/// over no argument, into the temporary, and steps one opline; the
/// row is the native's place in `zend_flf_functions`, which the
/// engine built from the bundle in registration order. No program
/// reaches this opcode under the oracle, since no native of php
/// 8.5.9 declares a nullary entry; this unit is that program.
#[test]
fn a_nullary_frameless_call_serves_the_native_into_its_temporary() {
    let engine = with_a_nullary_frameless_native();
    let row = engine
        .shared
        .frameless
        .iter()
        .position(|entry| entry.arity == 0)
        .and_then(|at| u32::try_from(at).ok())
        .expect("a nullary frameless row");
    assert_eq!(
        engine.frameless().get(row as usize).map(|n| n.0.as_ref()),
        Some(&b"seven"[..])
    );
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let unit = Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::FramelessIcall0,
                out_type: OperandKind::Tmp,
                out: 0,
                extended_value: row,
                ..Opline::default()
            }],
            num_tmps: 1,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let call = code.oplines.first().copied().expect("opline 0");
    assert_eq!(
        served!(req, frame, at => frameless_icall_0::<Nop>(&mut req, frame, code, &call, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(req.slot(frame, 0).as_int(), Some(7));
    assert_eq!(req.vm().opline(frame), 1);
    assert!(
        req.serving_call().is_none(),
        "the call is popped with the native"
    );
}

/// A row past the engine's table is the internal-error fatal that
/// names it, never a read past the end.
#[test]
fn a_row_the_table_has_not_is_an_internal_error() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let unit = Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::FramelessIcall0,
                out_type: OperandKind::Tmp,
                out: 0,
                extended_value: 1_000_000,
                ..Opline::default()
            }],
            num_tmps: 1,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let call = code.oplines.first().copied().expect("opline 0");
    assert!(matches!(
        served!(req, frame, at => frameless_icall_0::<Nop>(&mut req, frame, code, &call, at)),
        Err(fatal) if matches!(fatal.reason(), php_heap::FatalReason::Error(message) if message.starts_with("InternalError: ZEND_FRAMELESS_ICALL_0 indexes frameless entry 1000000"))
    ));
}

/// `zend_undefined_function_helper` names the constant as written
/// (`RT_CONSTANT(opline, opline->op2)`) where the lookup read the
/// lowered one after it, so the `Error` for `NoPe()` says `NoPe`;
/// the served witness beside [`init_fcall_by_name`] pins the whole
/// line against the oracle.
#[test]
fn a_call_by_name_of_no_function_raises_over_the_name_as_written() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = by_name(&mut strings);
    let mut oplines = unit.main.oplines.clone();
    let written = strings
        .intern(b"NoPe")
        .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string);
    let lowered = strings
        .intern(b"nope")
        .map_or(php_heap::Zval::UNDEF, php_heap::Zval::string);
    if let Some(init) = oplines.first_mut() {
        init.op2 = 3;
    }
    let unit = Unit {
        main: OpArray {
            oplines,
            literals: unit
                .main
                .literals
                .iter()
                .copied()
                .chain([written, lowered])
                .collect(),
            ..unit.main.clone()
        },
        ..Unit::default()
    }
    .published(None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let init = code.oplines.first().copied().expect("opline 0");

    assert_eq!(
        served!(req, frame, at => init_fcall_by_name::<Nop>(&mut req, frame, code, &init, at)),
        Ok(Stepped::Threw(frame))
    );
    assert_eq!(pending_message(&req), "Call to undefined function NoPe()");
}

/// The message of the throwable pending, empty where none is.
fn pending_message(req: &Request) -> String {
    req.pending()
        .and_then(|throwable| throwable.prop(req, b"message"))
        .and_then(|message| message.as_str_bytes().map(<[u8]>::to_vec))
        .map(|message| String::from_utf8_lossy(&message).into_owned())
        .unwrap_or_default()
}

/// A unit whose main op array has one CV, one temporary and one
/// `ASSIGN` of the constant `1` into the CV, its value unused:
/// `$a = 1;`.
fn assigning() -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::Assign,
                op1_type: OperandKind::Cv,
                op1: 0,
                op2_type: OperandKind::Const,
                op2: 0,
                out_type: OperandKind::Unused,
                ..Opline::default()
            }],
            literals: vec![php_heap::Zval::long(1)],
            num_cvs: 1,
            num_tmps: 1,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// `zend_assign_to_variable`'s first test (`Z/Zend/zend_execute.h:171`):
/// `ASSIGN` over a CV holding a reference writes the value the cell
/// holds, releases what the cell held after the write, and leaves the
/// cell the CV's, so a second CV bound to the same cell reads the new
/// value, which is what `$b = &$a; $a = 1;` shows once `&$x`
/// compiles. The temporary `result` names, under `RETURN_VALUE_USED`,
/// is the value and never the cell.
#[test]
fn an_assign_into_a_cv_holding_a_reference_writes_the_referent() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let unit = assigning();
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let mut op = code.oplines.first().copied().expect("opline 0");
    op.out_type = OperandKind::Tmp;
    // The temporary's slot, past the one CV, as the compiler folds it.
    op.out = 1;
    let text = req
        .string(b"in the cell")
        .expect("the string \"in the cell\"");
    let cell = req.reference(Value::string(text)).expect("a reference");
    let start = req.memory().usage(false);
    // Two CVs would share the cell; this frame has one, and the test
    // is the second holder.
    req.addref(&cell.as_value());
    req.replace_slot(frame, 0, cell.as_value());

    assert_eq!(
        served!(req, frame, at => assign::<Nop, crate::operand::Cv, crate::operand::Const>(&mut req, frame, code, &op, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(
        req.slot(frame, 0).as_php_ref(),
        Some(cell),
        "the CV still holds the cell"
    );
    assert_eq!(cell.get().as_int(), Some(1), "and the cell holds the value");
    assert_eq!(
        req.memory().usage(false),
        start - 40,
        "what the cell held went back to the heap"
    );
    assert_eq!(
        req.slot(frame, 1).as_int(),
        Some(1),
        "the result is the value, not the cell"
    );
    req.pop_frame();
    assert_eq!(
        cell.get().as_int(),
        Some(1),
        "the second holder keeps the cell"
    );
    req.release(cell.as_value());
    assert_eq!(req.memory().usage(false), start - 40 - 32);
}

/// #367, the first acceptance: a `foreach` by value over a plain
/// object walks the properties the scope may see, the private one
/// stepped over from outside the class, the key the name as
/// written. The witness beside [`fe_reset_r`] pins the same line
/// against the oracle; this holds the engine to it.
#[test]
fn a_foreach_over_an_object_walks_the_properties_the_scope_may_see() {
    let served = Engine::cli_builder().build().serve_capture(
        "<?php class P { public $a = 1; private $b = 2; public $c = 3; } foreach (new P as $k => $v) echo \"$k=$v \";",
        crate::test::Capture::default(),
    );
    served.assert_clean("a=1 c=3 ");
}

/// The same loop from inside the class sees every property, as
/// `zend_check_property_access` lets the declaring scope through.
#[test]
fn a_foreach_over_this_walks_the_private_properties_too() {
    let served = Engine::cli_builder().build().serve_capture(
        "<?php class C { public $n = 1; protected $p = 2; public $s = 'x'; private $q = 3; public function all() { foreach ($this as $k => $v) echo \"$k=$v \"; } } $c = new C; foreach ($c as $k => $v) echo \"$k=$v \"; $c->all();",
        crate::test::Capture::default(),
    );
    served.assert_clean("n=1 s=x n=1 p=2 s=x q=3 ");
}

/// #367, the second acceptance: an object of no properties runs the
/// body no time, and a property declared with no default is the
/// `null` `default_properties_table` put there, walked like any
/// other.
#[test]
fn a_foreach_over_an_object_of_no_properties_runs_its_body_no_time() {
    let served = Engine::cli_builder().build().serve_capture(
        "<?php foreach (new stdClass as $v) echo 'no'; class U { public $kept = 2; public $null; } foreach (new U as $k => $v) echo $k, ' ';",
        crate::test::Capture::default(),
    );
    served.assert_clean("kept null ");
}

/// The walk itself: every property `Exception` declares is
/// protected or private, so the global scope may see none of them
/// and `zend_fe_fetch_object_helper` steps over the whole table,
/// which is the jump past the loop.
#[test]
fn the_walk_over_an_object_of_properties_the_scope_may_not_see_finds_nothing() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let class = req
        .class_named(Bare::resolved(b"exception"))
        .expect("the engine declares Exception");
    let object =
        crate::api::call::new_object_made(&mut req, class).expect("Exception is instantiable");
    let idx = req
        .object_walk_start(object)
        .expect("the walk starts")
        .expect("Exception declares properties, so the walk is registered");
    assert!(
        matches!(req.object_walk(object, idx, false), Ok(None)),
        "the global scope sees none of Exception's properties"
    );
    req.heap_mut().iterator_del(idx);
    req.release(object.as_value());
}

/// A debug build stops at a write of a `TMP` over an owner the slot
/// still holds, #723's shape planted: a string left in the `TMP` a
/// `QM_ASSIGN` writes, as `FE_RESET_R` left its subject before #723,
/// is the `InternalError` naming the opcode, the opline and the
/// temporary, raised by the loop after the handler and before the
/// next one, and not a second release several allocations later.
#[cfg(debug_assertions)]
#[test]
fn a_write_over_a_temporary_that_still_holds_a_value_is_an_internal_error() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let unit = Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::QmAssign,
                op1_type: OperandKind::Const,
                op1: 0,
                out_type: OperandKind::Tmp,
                out: 0,
                lineno: 5,
                ..Opline::default()
            }],
            literals: vec![php_heap::Zval::long(7)],
            num_tmps: 1,
            filename: php_unit::decl::Name::from("planted.php"),
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let left = req
        .string(b"an owner left behind")
        .expect("the string \"an owner left behind\"");
    let planted = req.set_slot(frame, 0, Value::string(left));
    assert!(
        planted.to_zval().is_undef(),
        "the slot was empty before the plant"
    );

    assert_eq!(
        crate::tier::step(&mut req, frame),
        Err(Fatal::error(
            "InternalError: ZEND_QM_ASSIGN at opline 0 writes its TMP #0 over an owner it still holds (String): the handler that last read the slot moved it out and did not empty it"
        ))
    );
    assert_eq!(
        req.slot(frame, 0).as_int(),
        Some(7),
        "the write itself stood"
    );
    // Rendered at the line of the handler that wrote the temporary
    // (#2046).
    assert!(
        matches!(req.ending(), Some(crate::api::request::Ending::Fatal(logged)) if logged.contains(" in planted.php on line 5")),
        "{:?}",
        req.ending()
    );
    req.pop_frame();
}
