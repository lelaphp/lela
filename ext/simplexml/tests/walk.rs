//! The walks over the rows `sxe_get_properties` builds in place of an
//! element's table (#2122): what `PhpObject::property_table` answers for
//! a class that builds its own, which a foreach walk holds one owner of
//! each of, since the element's next build gives up the table they are
//! views of.
#![cfg(debug_assertions)]

use std::ops::ControlFlow;

use php_ext::abi::{Callable, Extension, ExtensionBuilder};
use php_ext::attr::php_function;
use php_ext::call::call;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::test::{Capture, Engine, LeakSensor};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpObject, PhpString, Value};

/// `built_rows(object $o, callable $f): string`: a foreach walk over the
/// object's properties that calls `$f` with each row's key, then reads
/// the row's value, a string's bytes.
#[php_function]
fn built_rows(req: &mut Request, o: PhpObject, f: Callable) -> Result<Owner<PhpString>, Thrown> {
    let table = o.property_table(req)?;
    let mut read = String::new();
    walk::foreach(req, table, Hold::None, |req, row| -> walk::Step {
        let key = match row.key() {
            Key::Int(n) => Value::int(*n),
            Key::Str(s) => Value::string(*s),
        };
        let answer = call(req, &f, &[key])?;
        answer.give_up(req);
        let bytes = row.value().as_str_bytes().unwrap_or_default();
        read.push_str(&String::from_utf8_lossy(bytes));
        read.push(' ');
        Ok(ControlFlow::Continue(()))
    })?;
    PhpString::new(req, read.as_bytes())
}

fn walks() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![built_rows::declared()])
        .build("sxe_walks")
}

/// Each step's code builds the element's table again (`get_object_vars`
/// over it, `sxe_get_properties`), which gives up the table the walk's
/// rows were read from; the walk's owners keep every row it has yet to
/// lend, and give them back as it ends, so the armed request ends with
/// nothing held.
#[test]
fn a_foreach_walk_holds_the_rows_a_class_builds() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_standard::extension(),
            php_ext_libxml::extension(),
            php_ext_simplexml::extension(),
            walks(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(
        "<?php $x = simplexml_load_string('<a><b>bbbbbbbbbbbbbbbbbbbbbbbb</b><c>cccccccccccccccccccccccc</c><d>dddddddddddddddddddddddd</d></a>');\n\
         echo built_rows($x, function ($k) use ($x) { get_object_vars($x); $GLOBALS['s' . $k] = str_repeat('z', 24); $GLOBALS['t' . $k] = str_repeat('y', 24); }), \"\\n\";\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "bbbbbbbbbbbbbbbbbbbbbbbb cccccccccccccccccccccccc dddddddddddddddddddddddd \n"
    );
    assert_eq!(served.fatal, None);
}
