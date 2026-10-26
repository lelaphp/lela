//! A class's cache slots: a catch, a static call, a method call, `new`,
//! `instanceof` and a class constant, each looking its class up once.

use super::*;

use crate::api::values::ClassId;

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
        let thrown = crate::api::raising::throw_error(&mut req, ClassId::ERROR, "e");
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
                Cached::ClassAndCallee { class, primed: exec::PrimedFrom { scope: None, .. }, function, .. }
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
                Cached::ClassAndCallee { class: held, primed: exec::PrimedFrom { scope: None, .. }, function: callee, .. }
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
    let thrown = crate::api::raising::throw_error(&mut req, ClassId::ERROR, "e");
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

/// `ZEND_FETCH_CLASS_CONSTANT` over two constants, twice: the first
/// pass reads the constant on its class and writes the class and the
/// value into the slot, `CACHE_POLYMORPHIC_PTR(opline->extended_value,
/// ce, value)`; the second serves what the slot holds, which a value
/// primed in its place shows.
#[test]
fn a_class_constant_over_two_constants_is_served_off_its_slot() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let mut strings = php_heap::Strings::new();
    let unit = Unit {
        main: OpArray {
            oplines: vec![Opline {
                opcode: Opcode::FetchClassConstant,
                op1_type: OperandKind::Const,
                op1: 0,
                op2_type: OperandKind::Const,
                op2: 2,
                out_type: OperandKind::Tmp,
                out: 0,
                ..Opline::default()
            }],
            literals: vec![
                interned(&mut strings, b"Attribute"),
                interned(&mut strings, b"attribute"),
                interned(&mut strings, b"TARGET_CLASS"),
            ],
            num_tmps: 1,
            cache_size: 16,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let op = code.oplines.first().copied().expect("opline 0");
    let class = req
        .class_named(Bare::resolved(b"attribute"))
        .expect("the engine declares Attribute");
    let fetched = |mut req: &mut Request| {
        req.vm_mut().stand(frame, 0);
        let edge = served!(req, frame, at => fetch_class_constant::<Nop>(req, frame, code, &op));
        assert_eq!(edge, Ok(Stepped::Same(frame)));
        req.slot(frame, 0).as_int()
    };
    assert_eq!(fetched(&mut req), Some(1), "Attribute::TARGET_CLASS");
    assert!(matches!(
        req.cached(frame, CacheSlot(0)),
        Cached::ClassConstant { class: held, value, .. } if held == class && value.as_int() == Some(1)
    ));
    let value = Value::int(99);
    req.prime_scoped(frame, CacheSlot(0), |primed| Cached::ClassConstant {
        class,
        primed,
        value,
    });
    assert_eq!(
        fetched(&mut req),
        Some(99),
        "the second pass reads the slot"
    );
}
