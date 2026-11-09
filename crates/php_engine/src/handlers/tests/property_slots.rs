//! A property's cache slot: a `??` on a declared property finding its
//! name once and reading the slot after.

use super::*;

/// A unit whose main op array is one `FETCH_OBJ_IS` over the CV `$o`
/// and the constant name `p` into the temporary past the CV, with the
/// three cache slots the compiler reserves for a property at byte zero:
/// the link `$o->p ?? …` reads.
fn coalescing(strings: &mut php_heap::Strings) -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![
                Opline {
                    opcode: Opcode::FetchObjIs,
                    op1_type: OperandKind::Cv,
                    op1: 0,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out_type: OperandKind::Tmp,
                    // The temporary's slot, past the one CV.
                    out: 1,
                    extended_value: 0,
                    ..Opline::default()
                },
                back(),
            ]
            .into(),
            literals: vec![interned(strings, b"p")].into(),
            vars: vec![php_unit::decl::Name::from("o")],
            num_cvs: 1,
            num_tmps: 1,
            // `zend_alloc_cache_slots(3)`: the class, the offset and the
            // property's info.
            cache_size: 24,
            ..OpArray::default()
        },
        ..Unit::default()
    }
    .published(None)
}

/// `ZEND_FETCH_OBJ_IS` over a constant name, twice on one object of a
/// class declaring the property, which is what `$o->p ?? 'd'` in a loop
/// is: the first pass finds the slot unprimed, so the kernel finds the
/// property by name, reads it and writes the class and the offset into
/// the slot; the second reads the declared slot at the cached offset
/// (`zobj->ce == CACHED_PTR_EX(cache_slot)`) and finds no name.
#[test]
fn a_coalesce_on_a_declared_property_finds_its_name_once() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    assert!(
        req.code_mut()
            .eval_stringl(b"class C { public $p = 1; }", b"declared")
            .is_ok(),
        "the class is declared"
    );
    let object = req.new_object_of("C").expect("an object of C");
    let mut strings = php_heap::Strings::new();
    let unit = coalescing(&mut strings);
    let frame = req.pushed(PublishedUnit::clone(&unit));
    let code = crate::exec::code_of(&unit);
    let op = code.oplines.first().copied().expect("opline 0");
    req.replace_slot(frame, 0, object.as_value());
    assert_eq!(req.cached(frame, CacheSlot(0)), Cached::Unprimed);
    let before = req.vm().lookups;

    for pass in 1..=2 {
        req.vm_mut().stand(frame, 0);
        assert_eq!(
            served!(req, frame, at => fetch_obj_is::<Nop>(&mut req, frame, code, &op)),
            Ok(Stepped::Same(frame)),
            "the fetch goes on on pass {pass}"
        );
        assert_eq!(
            req.slot(frame, 1).as_int(),
            Some(1),
            "the temporary holds the property on pass {pass}"
        );
        assert_eq!(
            req.vm().lookups - before,
            1,
            "one find by name over {pass} pass(es)"
        );
        assert!(
            matches!(
                req.cached(frame, CacheSlot(0)),
                Cached::Property(exec::PropertyPlace {
                    offset: exec::PropertyOffset::Declared { .. },
                    ..
                })
            ),
            "the class and a declared offset are in the slot after pass {pass}"
        );
    }
}
