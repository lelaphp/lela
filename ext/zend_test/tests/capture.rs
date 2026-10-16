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

#[test]
fn observe_function_names_set_by_a_request_moves_the_handlers() {
    let out = served(
        &[("enabled", "1"), ("observe_function_names", "f")],
        "<?php function f() {} function g() {} f(); g(); \
         ini_set('zend_test.observer.observe_function_names', 'g'); f(); g();",
    );
    assert_eq!(out.matches("<f>").count(), 1, "{out}");
    assert_eq!(out.matches("<g>").count(), 1, "{out}");
    assert!(out.rfind("<g>") > out.rfind("<f>"), "{out}");
}

#[test]
fn observe_declaring_names_each_function_and_class_declared() {
    let out = served(
        &[("enabled", "1"), ("observe_declaring", "1")],
        "<?php if (true) { function F() {} class C {} }",
    );
    assert!(out.contains("<!-- declared function 'f' -->"), "{out}");
    assert!(out.contains("<!-- declared class 'c' -->"), "{out}");
}

/// `zend_test.zend_mm_custom_handlers.enabled` is a request's to set and
/// takes the value it is set to, and observing writes past what the
/// request writes: what the program writes, `memory_get_usage()` among it, is
/// what it writes with the switch off.
#[test]
fn the_allocation_switch_is_taken_and_leaves_what_the_program_writes_alone() {
    let program = |on: u8| {
        format!(
            "<?php $m = memory_get_usage();\n\
             var_dump(ini_set('zend_test.zend_mm_custom_handlers.enabled', '{on}'), ini_get('zend_test.zend_mm_custom_handlers.enabled'));\n\
             $m = memory_get_usage(); $s = \"$m$m$m$m\"; $t = $s . $s; unset($s); gc_mem_caches();\n\
             echo memory_get_usage() - $m, \"\\n\";\n\
             ini_set('zend_test.zend_mm_custom_handlers.enabled', '0');\n"
        )
    };
    let off = served(&[], &program(0));
    let on = served(&[], &program(1));
    assert!(
        off.starts_with("string(1) \"0\"\nstring(1) \"0\"\n"),
        "{off}"
    );
    assert!(on.starts_with("string(1) \"0\"\nstring(1) \"1\"\n"), "{on}");
    assert_eq!(on.lines().nth(2), off.lines().nth(2), "{on}\n{off}");
}

/// `ext/opcache/tests/jit/ignored_opcodes.phpt`'s program, the one php
/// test that sets `show_opcode_in_user_handler` (design §7.1, revision
/// 107), served under its `--INI--` less opcache's: each silence opline
/// told before its handler, at the depth its frame stands, and the
/// program's own observation around them. The lines are the `zend-test`
/// oracle's under the same switches; the first and last name the
/// file, which differs.
#[test]
fn ignored_opcodes_tells_each_silence_opline_as_the_oracle_does() {
    let out = served(
        &[
            ("enabled", "1"),
            ("show_output", "1"),
            ("observe_all", "1"),
            (
                "show_opcode_in_user_handler",
                "ZEND_BEGIN_SILENCE, ZEND_END_SILENCE",
            ),
        ],
        "<?php\nfunction test(): int\n{\n    return 0;\n}\n\nexit(@test());\n?>\n",
    );
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.first().is_some_and(|l| l.starts_with("<!-- init '")),
        "{out}"
    );
    assert_eq!(
        lines.get(2..lines.len().saturating_sub(1)),
        Some(
            &[
                "  <!-- opcode: 'ZEND_BEGIN_SILENCE' in user handler -->",
                "  <!-- init test() -->",
                "  <test>",
                "  </test>",
                "  <!-- opcode: 'ZEND_END_SILENCE' in user handler -->",
                "  <!-- init exit() -->",
                "  <exit>",
                "    <!-- Exception: UnwindExit -->",
                "  </exit>",
                "  <!-- Exception: UnwindExit -->",
            ][..]
        ),
        "{out}"
    );
    assert!(
        lines.get(1).is_some_and(|l| l.starts_with("<file '")),
        "{out}"
    );
    assert!(
        lines.last().is_some_and(|l| l.starts_with("</file '")),
        "{out}"
    );
}

/// `zend_add_trait_method`'s copy of a native trait's method taken in
/// under an alias carries the alias as its `function_name`, so the
/// observer names it `Foo::aliased`, as the zend-test oracle prints it.
#[test]
fn an_aliased_native_trait_method_is_observed_by_its_alias() {
    let out = served(
        &[("enabled", "1"), ("observe_all", "1")],
        "<?php class Foo { use _ZendTestTrait { testMethod as aliased; } } (new Foo)->aliased();",
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.get(2..5),
        Some(
            &[
                "  <!-- init Foo::aliased() -->",
                "  <Foo::aliased>",
                "  </Foo::aliased>"
            ][..]
        ),
        "{out}"
    );
}
