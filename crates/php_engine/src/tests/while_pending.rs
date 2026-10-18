//! What a step inside [`crate::api::raising::while_pending`] answers
//! naming the pending throwable, which is no second owner (#2648,
//! #2858): php's `zend_call_function` returns at once while a throwable
//! is pending (`Z/Zend/zend_execute_API.c:822`), so a conversion that
//! would serve a `__toString` serves nothing and answers the pending
//! throwable itself; and a typed property's write refused while it is
//! pending raises nothing over it (`zend_verify_property_type_error`)
//! and answers it the same way.

use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::coerce::to_string;
use crate::api::raising::{Thrown, clear, throw, while_pending};
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
    let thrown = throw(req, "Exception", "first");
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
    let thrown = throw(req, "Exception", "first");
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
        .functions(vec![
            pending_conversion::declared(),
            pending_write::declared(),
        ])
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
    let stdout = String::from_utf8_lossy(&served.stdout);
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
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert_eq!(stdout, "caught first unset\n");
    assert_eq!(served.exit_code, 0, "{stdout}");
}
