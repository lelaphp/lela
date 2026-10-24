//! A function call's cache slots: a call by name looked up once and
//! read off its slot after, a request-only unit's, a namespaced pair's,
//! and a name nothing declares.

use super::*;

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
        served!(req, frame, at => send_val_ex::<Nop, crate::operand::Const>(&mut req, frame, code, &send, at)),
        Ok(Stepped::Same(frame))
    );
    assert_eq!(
        served!(req, frame, at => do_fcall_by_name::<Nop>(&mut req, frame, code, &call, at)),
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
fn called_twice(mut req: Box<Request>, unit: &PublishedUnit) {
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
