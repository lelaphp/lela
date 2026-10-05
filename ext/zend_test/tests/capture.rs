//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle, its switches
//! set as the host sets them, as a `.phpt`'s `--INI--` does.

use php_ext::test::{Capture, Engine};

/// What a program writes on an engine whose startup ini sets `switches`.
fn served(switches: &[(&str, &str)], source: &str) -> String {
    let mut builder = Engine::builder().extensions(&[php_ext_zend_test::extension()]);
    for (name, value) in switches {
        builder = builder.ini(format!("zend_test.observer.{name}"), value);
    }
    let served = builder.build().serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

#[test]
fn the_extension_is_loaded() {
    assert_eq!(
        served(&[], "<?php var_dump(extension_loaded('zend_test'));"),
        "bool(true)\n"
    );
}

#[test]
fn observe_all_writes_each_frame_begun_and_ended_with_what_it_returned() {
    let out = served(
        &[
            ("enabled", "1"),
            ("observe_all", "1"),
            ("show_return_value", "1"),
        ],
        "<?php function f($a) { return $a + 1; } echo f(1), \"\\n\";",
    );
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.first().is_some_and(|l| l.starts_with("<!-- init '")),
        "{out}"
    );
    assert_eq!(
        lines.get(2..6),
        Some(&["  <!-- init f() -->", "  <f>", "  </f:2>", "2"][..]),
        "{out}"
    );
    assert!(
        lines.last().is_some_and(|l| l.starts_with("</file '")),
        "{out}"
    );
}

#[test]
fn nothing_is_written_while_the_observer_is_not_enabled() {
    assert_eq!(
        served(
            &[("observe_all", "1")],
            "<?php function f() {} f(); echo 'done';"
        ),
        "done"
    );
}

#[test]
fn observe_function_names_observes_the_named_functions_alone() {
    let out = served(
        &[("enabled", "1"), ("observe_function_names", "g")],
        "<?php function f() {} function g() {} f(); g();",
    );
    assert!(out.contains("<g>\n</g>\n"), "{out}");
    assert!(!out.contains("<f>"), "{out}");
    assert!(out.contains("<!-- init f() -->"), "{out}");
}

#[test]
fn a_fiber_is_started_suspended_resumed_and_returns() {
    let out = served(
        &[
            ("enabled", "1"),
            ("show_output", "0"),
            ("fiber_switch", "1"),
        ],
        "<?php $f = new Fiber(function () { Fiber::suspend(); }); $f->start(); $f->resume();",
    );
    let tags: Vec<&str> = out
        .lines()
        .filter(|l| !l.starts_with("<!--"))
        .map(|l| l.split(' ').next().unwrap_or_default())
        .collect();
    assert_eq!(tags, ["<init", "<suspend", "<resume", "<returned"], "{out}");
}

#[test]
fn execute_internal_names_each_native_as_it_is_entered() {
    assert_eq!(
        served(
            &[("execute_internal", "1")],
            "<?php function f($a) { return function_exists($a); } echo f('f') ? 'y' : 'n';"
        ),
        "<!-- internal enter function_exists() -->\ny"
    );
}
