//! The owners the face takes for a native and gives up, as a program
//! meets them (#2057, design §5.1): each engine arms the request leak
//! sensor (#1992), so an owner the face took and kept ends the request
//! in the `InternalError` naming what the heap still held.

use php_macros::{php_class, php_function};

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// `enum Shade { case Light; case Dark; case Dim; }`, a native enum as
/// `RoundingMode` is one.
#[php_class]
#[derive(Debug)]
enum Shade {
    Light,
    Dark,
    Dim,
}

#[php_class]
impl Shade {}

/// `shade_turned(Shade $shade = Shade::Light): Shade`: the case
/// `$shade` is not, an enum argument read and an enum returned, as
/// `round()` reads its `RoundingMode` and `pcntl_getqos_class()` returns
/// its `QosClass`.
#[php_function]
fn shade_turned(#[php(default = Shade::Light)] shade: Shade) -> Shade {
    match shade {
        Shade::Light => Shade::Dark,
        Shade::Dark | Shade::Dim => Shade::Light,
    }
}

fn shades() -> Extension {
    ExtensionBuilder::default()
        .class::<Shade>()
        .functions(vec![shade_turned::declared()])
        .build("native_owners")
}

/// `src` served by a CLI engine with `extensions` and the leak sensor
/// armed: what it wrote, held to `expected`, and a clean exit.
fn served(extensions: &[Extension], src: &str, expected: &str) {
    let engine = Engine::cli_builder()
        .extensions(extensions)
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(src, Capture::default());
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert_eq!(stdout, expected);
    assert_eq!(served.exit_code, 0, "{stdout}");
}

/// An enum argument is read off the case's `name`, as
/// `zend_enum_fetch_case_name` reads it, and the case returned has the
/// one owner `RETURN_OBJ_COPY` takes, so neither the cases the argument
/// was matched against nor the returned case outlives the request
/// (#2059). The default is the request's to keep.
#[test]
fn an_enum_argument_and_return_keep_no_case() {
    served(
        &[shades()],
        "<?php\n\
         echo shade_turned(Shade::Dim)->name, ' ', shade_turned()->name, ' ', shade_turned(Shade::Dark) === Shade::Light ? 'same' : 'other', \"\\n\";\n",
        "Light Dark same\n",
    );
}
