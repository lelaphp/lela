//! A handler that gives up what an opline is reading or writing: each
//! program here is served with the leak sensor armed and ends the
//! request with status 0, what it wrote held, and with nothing leaked.

use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// `source` served with the leak sensor armed: what it wrote.
fn served(source: &str) -> String {
    let engine = Engine::cli_builder().sense_leaks(LeakSensor::Armed).build();
    let served = engine.serve_capture(source, Capture::default());
    let stdout = served.stdout_text();
    assert_eq!(served.exit_code, 0, "{stdout}");
    stdout
}

#[test]
fn a_cell_whose_names_a_conversion_gave_up_is_bound() {
    let stdout = served(
        "<?php\n\
         class S { function __toString(): string { unset($GLOBALS['x']); return 'conv'; } }\n\
         class T { public string $s = ''; public static string $t = ''; }\n\
         $o = new T; $x = new S; $o->s = &$x; echo $o->s, \"\\n\";\n\
         $x = new S; T::$t = &$x; echo T::$t, \"\\n\";\n",
    );
    assert_eq!(stdout, "conv\nconv\n");
}

#[test]
fn an_object_a_conversion_gave_up_stands_past_the_bind() {
    let stdout = served(
        "<?php\n\
         class S { function __toString(): string { $GLOBALS['o'] = null; return 'conv'; } }\n\
         class T { public string $s = ''; function __destruct() { echo $this->s, \"\\n\"; } }\n\
         $o = new T; $x = new S; $o->s = &$x; echo \"after\\n\";\n",
    );
    assert_eq!(stdout, "conv\nafter\n");
}

#[test]
fn a_returned_value_lands_in_the_table_the_notice_grew() {
    let stdout = served(
        "<?php\n\
         #[AllowDynamicProperties] class C {}\n\
         function f() { $v = ''; for ($i = 0; $i < 200; $i++) { $v .= 'v'; } return $v; }\n\
         $o = new C; $o->z = 1;\n\
         set_error_handler(function () { for ($i = 0; $i < 100; $i++) { $GLOBALS['o']->{\"p$i\"} = $i; } return true; });\n\
         $o->z = &f();\n\
         echo strlen($o->z), ' ', $o->p99, \"\\n\";\n",
    );
    assert_eq!(stdout, "200 99\n");
}

#[test]
fn a_returned_value_goes_with_the_object_the_notice_gave_up() {
    let stdout = served(
        "<?php\n\
         #[AllowDynamicProperties] class C { function __destruct() { echo strlen($this->z), \"\\n\"; } }\n\
         function f() { $v = ''; for ($i = 0; $i < 200; $i++) { $v .= 'v'; } return $v; }\n\
         set_error_handler(function () { $GLOBALS['o'] = null; return true; });\n\
         $o = new C; $o->z = &f();\n\
         echo $o === null ? \"gone\" : \"here\", \"\\n\";\n",
    );
    assert_eq!(stdout, "200\ngone\n");
}

#[test]
fn a_string_a_float_offset_gave_up_answers_as_no_string() {
    let stdout = served(
        "<?php\n\
         set_error_handler(function () { $GLOBALS['s'] = 1; return true; });\n\
         $p = 'ab'; $s = $p . 'cd'; echo isset($s[1.5]) ? 'set' : 'not set', \"\\n\";\n\
         $s = $p . 'cd'; echo empty($s[1.5]) ? 'empty' : 'not empty', \"\\n\";\n",
    );
    assert_eq!(stdout, "not set\nempty\n");
}

#[test]
fn a_property_step_stands_past_the_object_a_handler_gave_up() {
    let stdout = served(
        "<?php\n\
         #[AllowDynamicProperties] class C { public $s; }\n\
         set_error_handler(function () { $GLOBALS['o'] = null; return true; });\n\
         $p = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';\n\
         $o = new C; $o->s = $p . '!'; $x = $o->s++; echo strlen($x), \"\\n\";\n\
         $o = new C; $o->d = $p . '!'; $x = ++$o->d; echo strlen($x), \"\\n\";\n",
    );
    assert_eq!(stdout, "31\n31\n");
}
