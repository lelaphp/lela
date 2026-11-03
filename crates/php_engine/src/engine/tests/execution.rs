//! The loop end to end: declarations before the main op array, the call
//! sequence, the opcodes' writes and throws, and where the loop asks
//! the tier.

use super::*;
use php_heap::{Fatal, FrameRef};

use crate::observer::Untold;
use crate::probe::{Asked, Probe};
use crate::tier::Site;

/// A class the compiler bound with no opline (`zend_compile_class_decl`
/// under `toplevel`) is declared into the request's table before the
/// main op array runs, so the source the doc-comment witness of
/// `php_syntax` declares runs (#257). A name the table already holds
/// is no early bind: its `DECLARE_CLASS` raises
/// `zend_class_redeclaration_error` where it stands, after what ran
/// before it, in the wording for an internal class, or naming the
/// earlier declaration's site for one of the file's. Both are
/// `E_COMPILE_ERROR` at the class's line, over `#0 {main}` under
/// php's default `fatal_error_backtraces`, which is what the pinned
/// oracle writes for `<?php\necho 1;\nclass Exception {}` under
/// `php -n`.
#[test]
fn a_unit_s_classes_are_declared_before_its_main_op_array_runs() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(
        &engine,
        "<?php\nclass M { /** P */ public $x, $y; /** K */ const A = 1, B = 2; }\necho 1;",
    );
    assert_eq!((body.as_str(), observables.exit_code), ("1", 0));
    let (body, observables) = served(&engine, "<?php\necho 1;\nclass Exception {}");
    assert_eq!(
        (body.as_str(), observables.exit_code),
        (
            "1\nFatal error: Cannot redeclare class Exception in witness.php on line 3\nStack trace:\n#0 {main}\n",
            255
        )
    );
    let (body, observables) = served(&engine, "<?php\necho 1;\nclass M {}\nclass m {}");
    assert_eq!(
        (body.as_str(), observables.exit_code),
        (
            "1\nFatal error: Cannot redeclare class M (previously declared in witness.php:3) in witness.php on line 4\nStack trace:\n#0 {main}\n",
            255
        ),
        "php emits a DECLARE_CLASS for the second and fails it at run time"
    );
}

#[test]
fn an_engine_with_no_extensions_serves_what_uses_zend_alone() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(&engine, "<?php echo 1;");
    assert_eq!(body, "1");
    assert_eq!(observables.exit_code, 0);
    assert_eq!(observables.fatal(), None);
}

/// The same request through the `Capture` sink, which is the shape
/// every extension's own test reads (design §4.3 seam 4, §4.10).
#[test]
fn the_capture_sink_holds_what_a_zend_only_request_wrote() {
    let served = Engine::cli_builder()
        .build()
        .serve_capture("<?php echo 1;", crate::test::Capture::default());
    served.assert_clean("1");
    let served = Engine::cli_builder().build().serve_capture(
        "<?php var_dump(extension_loaded('Core'));",
        crate::test::Capture::default(),
    );
    assert_eq!(served.stdout_text(), "bool(true)\n");
}

#[test]
fn the_call_sequence_reaches_a_native_and_writes_what_it_returned() {
    let engine = Engine::cli_builder().build();
    let (body, _) = served(&engine, "<?php var_dump(extension_loaded('Core'));");
    assert_eq!(body, "bool(true)\n");
    let (body, _) = served(&engine, "<?php var_dump(extension_loaded('nope'));");
    assert_eq!(body, "bool(false)\n");
}

/// The four arithmetic opcodes end to end, each snippet the one its
/// kernel's witness pins, so that what the fences claim is what this
/// engine writes and not only what the oracle wrote.
#[test]
fn the_arithmetic_opcodes_write_what_their_witnesses_pin() {
    let engine = Engine::cli_builder().build();
    for (source, pinned) in [
        (
            "<?php echo 1 + (2 + extension_loaded('Core')), \"|\", 1.5 + (2.5 + extension_loaded('Core')), \"|\", \"1\" + (1 + extension_loaded('Core')), \"|\", 9223372036854775807 + (1 + extension_loaded('Core'));",
            "4|5|3|9.2233720368548E+18",
        ),
        (
            "<?php echo 1 - (2 - extension_loaded('Core')), \"|\", 1.5 - (2.5 - extension_loaded('Core')), \"|\", \"3\" - (1 + extension_loaded('Core')), \"|\", -9223372036854775807 - (1 + extension_loaded('Core'));",
            "0|0|1|-9.2233720368548E+18",
        ),
        (
            "<?php echo 2 * (3 * extension_loaded('Core')), \"|\", 1.5 * (2.5 * extension_loaded('Core')), \"|\", \"3\" * (1 + extension_loaded('Core')), \"|\", 4611686018427387904 * (1 + extension_loaded('Core'));",
            "6|3.75|6|9.2233720368548E+18",
        ),
        (
            "<?php echo 6 / (1 + extension_loaded('Core')), \"|\", 7.5 / (1.5 * extension_loaded('Core')), \"|\", \"7\" / (1 + extension_loaded('Core')), \"|\", (-9223372036854775807 - extension_loaded('Core')) / (0 - extension_loaded('Core'));",
            "3|5|3.5|9.2233720368548E+18",
        ),
    ] {
        let (body, observables) = served(&engine, source);
        assert_eq!(body, pinned, "{source}");
        assert_eq!(observables.fatal(), None, "{source}");
    }
}

/// `div_function`: the divisor of zero php refuses to fold, so the
/// opcode throws `DivisionByZeroError` where it stands, on line 1 of
/// a source whose `<?php` and whose `echo` share it. The witness on
/// [`crate::ops::div`] pins the oracle's rendering of the display branch;
/// what this holds is the log branch's spelling of the same fatal,
/// the status, and the double divisor the witness does not reach.
#[test]
fn a_division_by_zero_throws_php_s_error_where_the_opcode_stands() {
    let engine = Engine::cli_builder().build();
    let (_, observables) = served(&engine, "<?php echo 1/0;");
    assert_eq!(
        observables.fatal(),
        Some(
            "PHP Fatal error:  Uncaught DivisionByZeroError: Division by zero \
             in witness.php:1\nStack trace:\n#0 {main}\n  thrown in witness.php on line 1"
        )
    );
    assert_eq!(observables.exit_code, 255);
    let (_, observables) = served(&engine, "<?php echo 1/0.0;");
    assert_eq!(
        observables
            .fatal()
            .map(|text| text.contains("Uncaught DivisionByZeroError: Division by zero")),
        Some(true)
    );
}

/// `zend_binop_error`: an operand php will not read as a number is a
/// `TypeError` naming php's types around the operator as written, and
/// one with a number in front of it is the warning and the number.
#[test]
fn an_operand_php_refuses_to_read_as_a_number_is_php_s_type_error() {
    let engine = Engine::cli_builder().build();
    let (_, observables) = served(&engine, "<?php echo \"abc\" + extension_loaded('Core');");
    assert_eq!(
        observables
            .fatal()
            .map(|text| text
                .contains("Uncaught TypeError: Unsupported operand types: string + bool")),
        Some(true),
        "{:?}",
        observables.fatal()
    );
    let (body, observables) = served(
        &engine,
        "<?php echo \"1abc\" + (1 + extension_loaded('Core'));",
    );
    assert_eq!(
        body,
        "\nWarning: A non-numeric value encountered in witness.php on line 1\n3"
    );
    assert_eq!(observables.fatal(), None);
}

#[test]
fn echo_writes_each_part_as_php_converts_it() {
    let engine = Engine::cli_builder().build();
    let (body, _) = served(
        &engine,
        "<?php echo 1, \"|\", true, \"|\", null, \"|\", 2.5;",
    );
    assert_eq!(body, "1|1||2.5");
}

/// `zend_exception_error` through `php_error_cb`: the four lines the
/// witness beside [`Request::uncaught`] pins against the oracle, with
/// the throw site in php's two spellings.
#[test]
fn a_call_of_a_name_nothing_declares_ends_the_request_as_php_names_it() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(&engine, "<?php\nnope();");
    assert_eq!(
        body,
        "\nFatal error: Uncaught Error: Call to undefined function nope() \
         in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2\n"
    );
    assert_eq!(
        observables.fatal(),
        Some(
            "PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
             in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
        )
    );
    assert_eq!(observables.exit_code, 255);
}

/// A throwable a native left pending names the native's call as its
/// site, and the native's own frame stands above `{main}`.
#[test]
fn a_throwable_a_native_left_pending_names_the_natives_call() {
    let engine = Engine::cli_builder().build();
    let (body, observables) = served(&engine, "<?php\nvar_dump();");
    assert_eq!(
        body,
        "\nFatal error: Uncaught ArgumentCountError: var_dump() expects at least 1 argument, \
         0 given in witness.php:2\nStack trace:\n#0 witness.php(2): var_dump()\n#1 {main}\n  \
         thrown in witness.php on line 2\n"
    );
    assert_eq!(observables.exit_code, 255);
}

#[test]
fn an_opcode_with_no_handler_ends_the_request_naming_it() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), description(""));
    let unit = Unit {
        main: php_unit::OpArray {
            oplines: vec![
                php_unit::Opline {
                    opcode: php_unit::Opcode::UserOpcode,
                    ..php_unit::Opline::default()
                },
                php_unit::Opline {
                    opcode: php_unit::Opcode::Return,
                    ..php_unit::Opline::default()
                },
            ]
            .into(),
            ..php_unit::OpArray::default()
        },
        ..Unit::default()
    };
    let frame = req.pushed(unit.published(None));
    assert_eq!(
        crate::tier::execute::<Nop, Untold>(&mut req, frame),
        Err(Fatal::error(
            "InternalError: ZEND_USER_OPCODE has no handler in this engine yet"
        ))
    );
}

/// Design §4.3 seam 8: `Site::Entry` is raised for the entry frame of
/// an `execute` whatever the caller is, and `Site::Return` for no
/// entry frame, which the stack's entry slot says.
#[test]
fn the_loop_asks_the_tier_at_the_entry_and_at_no_entry_frames_return() {
    let engine = Engine::cli_builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), description(""));
    let unit = engine
        .unit(b"<?php echo 1;", b"witness.php")
        .map_or_else(Unit::default, |unit| Unit::clone(&unit));
    let frame = req.pushed(unit.published(None));
    assert_eq!(
        crate::tier::execute::<Probe, Untold>(&mut req, frame),
        Ok(())
    );
    assert_eq!(
        req.state::<Asked>().sites,
        vec![(Site::Entry, frame)],
        "the entry frame is activated, and its own return raises no site"
    );
}

/// Design §5.6, §8: a user frame is activated after its `RECV*`
/// prologue, so `Probe` sees `(Entry, callee)` once per call and
/// `Return` once per callee frame, in the caller it returned to
/// (#4056); a call whose `RECV` throws is
/// reported `Threw` before the activation, so no `Entry` is raised
/// for it (#309).
#[test]
fn a_call_activates_its_frame_once_and_a_recv_that_throws_never_does() {
    let engine = Engine::cli_builder().tier::<Probe>().build();
    let mut req = Request::new(Arc::clone(&engine.shared), description(""));
    let unit = engine
        .unit(
            b"<?php function f($a, $b = 1) { return $a; } f(1); f(2, 3); f();",
            b"witness.php",
        )
        .unwrap_or_else(|| Unit::default().published(None));
    req.declare_functions(&unit);
    let frame = req.pushed(unit);
    let outcome = crate::tier::execute::<Probe, Untold>(&mut req, frame);
    assert!(
        outcome.is_ok() && req.is_pending(),
        "the third call's RECV throws and nothing catches it: it stays pending for the caller"
    );
    let callee = FrameRef(frame.0 + 1);
    assert_eq!(
        req.state::<Asked>().sites,
        vec![
            (Site::Entry, frame),
            (Site::Entry, callee),
            (Site::Return(crate::tier::SafepointIdx(1)), frame),
            (Site::Entry, callee),
            (Site::Return(crate::tier::SafepointIdx(1)), frame),
        ],
        "two calls activated and returned, the third never activated"
    );
}

/// `array_map` over one array as ext/standard maps it, for the test
/// below: the callback called back for each element through
/// [`crate::api::call::call`], php's `zend_call_function`, so each call
/// enters the loop afresh (`tier::reenter`). Its answers are dropped.
#[php_macros::php_function]
fn array_map(
    req: &mut Request,
    callback: crate::api::abi::Callable,
    array: crate::api::values::PhpArray,
) -> Result<(), crate::api::raising::Thrown> {
    use crate::api::values::walk;
    walk::foreach(req, array, walk::Hold::None, |req, row| -> walk::Step {
        let answer = crate::api::call::call(req, &callback, &[*row.value()])?;
        answer.give_up(req);
        Ok(std::ops::ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// An unbounded recursion through a native's callback is refused at
/// the loop's entry with php's stack-limit `Error`, on the test
/// harness's own thread, in the debug build too, whose loop frame,
/// every handler in line, is many times `zend.reserved_stack_size`:
/// the entry counts that frame below the one it is asked from, so the
/// frame is never laid past the thread's end (#3830). The number is
/// the thread's, so it is masked; the depth is whatever the thread
/// held.
#[test]
fn an_unbounded_recursion_through_array_map_is_refused_not_overflowed() {
    let engine = Engine::cli_builder()
        .additional_extension(
            crate::api::abi::ExtensionBuilder::default()
                .functions(&[array_map::DECLARED])
                .build("standard"),
        )
        .build();
    let (body, observables) = served(
        &engine,
        "<?php\n$depth = 0;\n$down = function () use (&$down, &$depth) { $depth++; return array_map($down, [0]); };\ntry { $down(); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\necho $depth, \"\\n\";",
    );
    let (sentence, depth) = body.split_once('\n').unwrap_or_default();
    let (said, number) = (
        sentence.split(" bytes").next().unwrap_or_default(),
        sentence.split_once(" bytes").map(|(_, rest)| rest),
    );
    assert_eq!(
        said.strip_prefix("Maximum call stack size of ")
            .map(|n| n.parse::<u64>().is_ok()),
        Some(true),
        "{body}"
    );
    assert_eq!(
        number,
        Some(
            " (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?"
        ),
        "{body}"
    );
    assert!(
        depth.trim_end().parse::<u32>().is_ok_and(|depth| depth > 1),
        "{body}"
    );
    assert_eq!(observables.exit_code, 0);
}
