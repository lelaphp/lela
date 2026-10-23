//! Safepoints a handler raises: a back-edge of a jump, a conditional
//! jump and a fused comparison, the interrupt tested there, and a
//! return.

use super::*;

use crate::probe::{Asked, Probe};

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
        served!(req, frame, at => is_smaller::<Probe, crate::operand::Const, crate::operand::Const, branch::OnJump<true, true>>(
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
