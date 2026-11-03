//! Writes to a slot: an assignment through a reference a CV holds, and
//! a temporary written over an owner it still holds.

use super::*;

/// A unit whose main op array has one CV, one temporary and one
/// `ASSIGN` of the constant `1` into the CV, its value unused:
/// `$a = 1;`.
fn assigning() -> PublishedUnit {
    Unit {
        main: OpArray {
            oplines: vec![
                Opline {
                    opcode: Opcode::Assign,
                    op1_type: OperandKind::Cv,
                    op1: 0,
                    op2_type: OperandKind::Const,
                    op2: 0,
                    out_type: OperandKind::Unused,
                    ..Opline::default()
                },
                back(),
            ]
            .into(),
            literals: vec![php_heap::Zval::long(1)].into(),
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
        served!(req, frame, at => assign::<Nop, crate::operand::Cv, crate::operand::Const, crate::operand::RetvalUsed>(&mut req, frame, code, &op, at)),
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
            oplines: vec![
                Opline {
                    opcode: Opcode::QmAssign,
                    op1_type: OperandKind::Const,
                    op1: 0,
                    out_type: OperandKind::Tmp,
                    out: 0,
                    lineno: 5,
                    ..Opline::default()
                },
                back(),
            ]
            .into(),
            literals: vec![php_heap::Zval::long(7)].into(),
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
