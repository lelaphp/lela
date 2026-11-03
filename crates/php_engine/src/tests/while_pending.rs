//! What a step inside [`crate::api::raising::while_pending`] answers
//! naming the pending throwable, which is no second owner (#2648,
//! #2858): php's `zend_call_function` returns at once while a throwable
//! is pending (`Z/Zend/zend_execute_API.c:822`), so a conversion that
//! would serve a `__toString` serves nothing and answers the pending
//! throwable itself; and a typed property's write refused while it is
//! pending raises nothing over it (`zend_verify_property_type_error`)
//! and answers it the same way. A throw the handler `set_exception_handler()`
//! set took with no frame standing leaves nothing pending, and the face goes on
//! past it as php's C goes on with `EG(exception)` empty (#3906).

use crate::api::values::ClassId;
use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::coerce::to_string;
use crate::api::raising::{Carrying, Cause, Level, Thrown, clear, raise, throw, while_pending};
use crate::api::request::Request;
use crate::api::values::{Owner, PhpObject, PhpString, PropertyScope, Value};
use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// `pending_conversion(object $o): string`: throws `first`, converts
/// `$o` while it is pending, and hands `first` on where the conversion
/// answered it, `"converted"` where the conversion succeeded and
/// `"another"` where it answered another throwable.
#[php_function]
fn pending_conversion(req: &mut Request, o: Value) -> Result<Owner<PhpString>, Thrown> {
    let thrown = throw(req, ClassId::EXCEPTION, "first");
    let converted = while_pending(req, &thrown, |req| to_string(req, &o));
    match converted {
        Some(Err(answered)) if answered.exception() == thrown.exception() => Err(thrown),
        Some(Ok(text)) => {
            text.give_up(req);
            clear(req, thrown)?;
            PhpString::new(req, b"converted")
        }
        _ => {
            clear(req, thrown)?;
            PhpString::new(req, b"another")
        }
    }
}

/// `pending_write(object $o): string`: throws `first`, writes the
/// string `"x"` to `$o->n` while it is pending, and hands `first` on
/// where the refused write answered it, as [`pending_conversion`] does.
#[php_function]
fn pending_write(req: &mut Request, o: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    let thrown = throw(req, ClassId::EXCEPTION, "first");
    let name = PhpString::new(req, b"n")?;
    let value = PhpString::new(req, b"x")?.into_value();
    let wrote = while_pending(req, &thrown, |req| {
        o.write_prop_in(req, PropertyScope::Declaring, name, value)
    });
    match wrote {
        Some(Err(answered)) if answered.exception() == thrown.exception() => Err(thrown),
        Some(Ok(())) => {
            clear(req, thrown)?;
            PhpString::new(req, b"written")
        }
        _ => {
            clear(req, thrown)?;
            PhpString::new(req, b"another")
        }
    }
}

fn pending() -> Extension {
    ExtensionBuilder::default()
        .functions(&[pending_conversion::DECLARED, pending_write::DECLARED])
        .build("while_pending")
}

/// The `__toString` is not served and the conversion answers the
/// pending `first`, which the native hands on as its one owner: the
/// leak sensor finds nothing kept, and the program catches `first`.
#[test]
fn a_call_inside_the_step_answers_the_pending_throwable() {
    let engine = Engine::cli_builder()
        .extensions(&[pending()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(
        "<?php\n\
         class T { function __toString(): string { echo \"ran\\n\"; return 't'; } }\n\
         try { echo pending_conversion(new T), \"\\n\"; } catch (Exception $e) { echo 'caught ', $e->getMessage(), \"\\n\"; }\n",
        Capture::default(),
    );
    let stdout = served.stdout_text();
    assert_eq!(stdout, "caught first\n");
    assert_eq!(served.exit_code, 0, "{stdout}");
}

/// The typed property's write is refused and answers the pending
/// `first` with no owner of its own, as the call does: the native drops
/// the answer and hands `first` on as its one owner, the leak sensor
/// finds nothing kept, and the property stays uninitialized.
#[test]
fn a_refused_write_inside_the_step_answers_the_pending_throwable() {
    let engine = Engine::cli_builder()
        .extensions(&[pending()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(
        "<?php\n\
         class T { public int $n; }\n\
         $t = new T;\n\
         try { echo pending_write($t), \"\\n\"; } catch (Exception $e) { echo 'caught ', $e->getMessage(), ' ', isset($t->n) ? 'set' : 'unset', \"\\n\"; }\n",
        Capture::default(),
    );
    let stdout = served.stdout_text();
    assert_eq!(stdout, "caught first unset\n");
    assert_eq!(served.exit_code, 0, "{stdout}");
}

/// A request-end function, served with no frame standing: each of
/// `first`, `second` and `third` is handed to the program's handler as
/// it is thrown, and the native goes on past each as php's C goes on
/// with `EG(exception)` empty: [`while_pending`] serves its step,
/// [`Carrying`] keeps nothing, and [`Thrown::chain`] answers what stood
/// before the throw the handler took.
fn taken_past(req: &mut Request) {
    let first = throw(req, ClassId::EXCEPTION, "first");
    let taken = first.cause(req) == Cause::Taken;
    while_pending(req, &first, |req| {
        raise(req, Level::Warning, &format!("stepped, taken {taken}"));
    });
    let mut carrying = Carrying::default();
    let served = carrying.serve(req, |req| {
        Err::<(), _>(throw(req, ClassId::EXCEPTION, "second"))
    });
    let kept = carrying.take().is_some();
    raise(
        req,
        Level::Warning,
        &format!("served {}, kept {kept}", served.is_ok()),
    );
    let third = throw(req, ClassId::EXCEPTION, "third");
    let chained = first.chain(req, third).cause(req) == Cause::Taken;
    raise(req, Level::Warning, &format!("chained taken {chained}"));
}

/// php serves `RSHUTDOWN` with `EG(current_execute_data)` empty, so
/// `zend_throw_exception_internal` hands each throw to the handler
/// `set_exception_handler()` set and returns with nothing pending
/// (`Z/Zend/zend_exceptions.c:209-221`): every step after it is served.
#[test]
fn a_throw_the_handler_took_with_no_frame_standing_is_gone_on_past() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .rshutdown(taken_past)
            .build("taken_past")])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(
        "<?php set_exception_handler(function ($e) { echo 'handler: ', $e->getMessage(), \"\\n\"; }); echo \"end\\n\";",
        Capture::default(),
    );
    let stdout = served.stdout_text();
    assert_eq!(
        stdout,
        "end\nhandler: first\n\nWarning: stepped, taken true in Unknown on line 0\n\
         handler: second\n\nWarning: served true, kept false in Unknown on line 0\n\
         handler: third\n\nWarning: chained taken true in Unknown on line 0\n"
    );
    assert_eq!(served.exit_code, 0, "{stdout}");
}
