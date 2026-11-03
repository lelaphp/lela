//! Frameless calls: the jump that primes a hit or a miss, a nullary
//! call into its temporary, and a row the table has not.

use super::*;

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
                back(),
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
    use crate::api::__private::{Frameless, NativeFunction};
    use crate::api::abi::ExtensionBuilder;
    fn seven(_: &mut Request) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::int(7))
    }
    #[allow(clippy::trivially_copy_pass_by_ref)] // the frameless table's entry type, `Frameless::Zero`
    fn frameless_seven(_: &mut Request, _: &[Value; 0]) -> Result<Owner<Value>, Thrown> {
        Ok(Owner::int(7))
    }
    const SEVEN: NativeFunction = NativeFunction {
        name: b"seven",
        parameters: &[],
        return_type: || None,
        frameless: &[Frameless::Zero(frameless_seven)],
        attributes: &[],
        entry: seven,
    };
    let extension = ExtensionBuilder::default()
        .functions(&[SEVEN])
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
            oplines: vec![
                Opline {
                    opcode: Opcode::FramelessIcall0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    extended_value: row,
                    ..Opline::default()
                },
                back(),
            ],
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
            oplines: vec![
                Opline {
                    opcode: Opcode::FramelessIcall0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    extended_value: 1_000_000,
                    ..Opline::default()
                },
                back(),
            ],
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
