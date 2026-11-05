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

#[test]
fn an_unset_past_the_table_a_handler_gave_up_deletes_nothing() {
    let stdout = served(
        "<?php\n\
         set_error_handler(function () { $GLOBALS['a'] = 1; return true; });\n\
         function tbl() { $x = []; for ($i = 0; $i < 40; $i++) { $x[] = 'x' . $i; } return $x; }\n\
         $a = tbl(); unset($a[$u]); echo $a, \"\\n\";\n",
    );
    assert_eq!(stdout, "1\n");
}

#[test]
fn an_unset_under_a_table_a_handler_gave_up_reads_no_entry_of_it() {
    let stdout = served(
        "<?php\n\
         set_error_handler(function () { $GLOBALS['a'] = 1; return true; });\n\
         function row() { $x = []; for ($i = 0; $i < 40; $i++) { $x[] = 'x' . $i; } return $x; }\n\
         $a = row(); $a[1] = row(); unset($a[1][$u]); echo $a, \"\\n\";\n",
    );
    assert_eq!(stdout, "1\n");
}

#[test]
fn a_test_past_the_table_a_handler_gave_up_finds_nothing() {
    let stdout = served(
        "<?php\n\
         set_error_handler(function () { $GLOBALS['a'] = 1; return true; });\n\
         function tbl() { $x = ['' => 'e']; for ($i = 0; $i < 40; $i++) { $x[] = 'x' . $i; } return $x; }\n\
         $a = tbl(); echo isset($a[$u]) ? 'set' : 'not set', \"\\n\";\n\
         $a = tbl(); echo empty($a[$u]) ? 'empty' : 'not empty', \"\\n\";\n",
    );
    assert_eq!(stdout, "not set\nempty\n");
}

/// The table a write held goes first and takes the key's last owner with
/// it, and a destructor it serves lays another key down in that block, so
/// the key is held past the table to ask what `$a` holds now.
#[test]
fn a_write_finds_no_entry_by_a_key_a_handler_gave_up() {
    let stdout = served(
        "<?php\n\
         class D { function __destruct() { $GLOBALS['a']['n' . $GLOBALS['j']] = 'kept'; } }\n\
         set_error_handler(function () { unset($GLOBALS['a'][$GLOBALS['k']]); $GLOBALS['a'][9] = 1; $GLOBALS['k'] = 0; return true; });\n\
         $i = 7; $j = 8; $k = 'n' . $i; $a = [$k => 1, 9 => new D];\n\
         $a[$k] = $u; foreach ($a as $at => $v) { echo $at, \"=\", $v, \"\\n\"; }\n",
    );
    assert_eq!(stdout, "9=1\nn8=kept\n");
}

/// The table going serves a destructor that moves what the handler's
/// write left, so the entry is found after it.
#[test]
fn a_write_finds_its_entry_past_a_destructor_of_the_table_it_held() {
    let stdout = served(
        "<?php\n\
         class D { function __destruct() { for ($i = 0; $i < 200; $i++) { $GLOBALS['a'][\"g$i\"] = $i; } } }\n\
         set_error_handler(function () { $GLOBALS['a'][1] = 1; return true; });\n\
         $i = 7; $a = ['x' . $i, new D]; $a[0] = $u;\n\
         $n = 0; foreach ($a as $v) { $n++; } echo $n, $a[0] === null ? ' null' : ' kept', \"\\n\";\n",
    );
    assert_eq!(stdout, "202 null\n");
}

#[test]
fn a_typed_cell_a_conversion_gave_up_takes_nothing() {
    let stdout = served(
        "<?php\n\
         class P { public string $s = 'a'; }\n\
         class T { function __toString(): string { $GLOBALS['a'] = 1; $GLOBALS['p'] = null; return 'conv'; } }\n\
         $p = new P; $a = [0]; $a[0] = &$p->s;\n\
         echo $a[0] = new T, '|', $a, '|', $p === null ? 'gone' : 'here', \"\\n\";\n",
    );
    assert_eq!(stdout, "conv|1|gone\n");
}

#[test]
fn a_write_goes_with_the_table_a_handler_gave_up() {
    let stdout = served(
        "<?php\n\
         set_error_handler(function () { $GLOBALS['a'] = 1; return true; });\n\
         function tbl() { $x = []; for ($i = 0; $i < 40; $i++) { $x[] = 'x' . $i; } return $x; }\n\
         $a = tbl(); echo $a[0] = $u, '|', $a, \"\\n\";\n\
         $a = tbl(); echo $a[0] .= [], '|', $a, \"\\n\";\n\
         $a = tbl(); $a[5] = ['q' . 'r']; $a[5][0] .= []; echo $a, \"\\n\";\n",
    );
    assert_eq!(stdout, "|1\nx0Array|1\n1\n");
}

#[test]
fn an_object_a_handler_gave_up_goes_past_its_compound_step() {
    let stdout = served(
        "<?php\n\
         class C { public $p = 'pp'; function __destruct() { echo $this->p, \"\\n\"; } }\n\
         set_error_handler(function () { $GLOBALS['o'] = null; return true; });\n\
         $o = new C; $o->p .= []; echo \"after\\n\";\n",
    );
    assert_eq!(stdout, "ppArray\nafter\n");
}

#[test]
fn a_cycle_a_collection_read_past_under_a_step_stays_a_possible_root() {
    let stdout = served(
        "<?php\n\
         class C { public $p = 'pp'; public $self; function __destruct() { echo 'dtor ', $this->p, \"\\n\"; } }\n\
         set_error_handler(function () { $GLOBALS['o'] = null; gc_collect_cycles(); return true; });\n\
         $o = new C; $o->self = $o; $o->p .= []; echo \"after\\n\";\n\
         echo gc_collect_cycles(), \"\\nend\\n\";\n",
    );
    assert_eq!(stdout, "after\ndtor ppArray\n1\nend\n");
}

#[test]
fn a_collection_under_a_step_that_left_the_count_as_it_stood_keeps_the_cycle_a_possible_root() {
    let stdout = served(
        "<?php\n\
         class C { public $p = true; public $self; public $self2; function __destruct() { echo \"dtor\\n\"; } }\n\
         set_error_handler(function () { $GLOBALS['o']->self2 = $GLOBALS['o']; $GLOBALS['o'] = null; gc_collect_cycles(); return true; });\n\
         $o = new C; $o->self = $o; $o->p++; echo \"after\\n\";\n\
         echo gc_collect_cycles(), \"\\nend\\n\";\n",
    );
    assert_eq!(stdout, "after\ndtor\n1\nend\n");
}

#[test]
fn a_step_that_cost_the_object_no_owner_adds_no_possible_root() {
    let stdout = served(
        "<?php\n\
         class C { public $p = 'pp'; }\n\
         $o = new C; $o->p .= 'x'; $o->p .= 'y'; echo gc_status()['roots'], \"\\n\";\n",
    );
    assert_eq!(stdout, "0\n");
}

#[test]
fn a_throw_over_a_compound_step_is_the_previous_of_the_destructor_it_serves() {
    let stdout = served(
        "<?php\n\
         class C { public $p = 'pp'; function __destruct() { throw new Exception('dtor'); } }\n\
         set_error_handler(function () { $GLOBALS['o'] = null; throw new Exception('handler'); });\n\
         try { $o = new C; $o->p .= []; }\n\
         catch (Exception $e) { echo $e->getMessage(), ' over ', $e->getPrevious()->getMessage(), \"\\n\"; }\n",
    );
    assert_eq!(stdout, "dtor over handler\n");
}

#[test]
fn a_right_operand_a_handler_gave_up_is_read_past_the_left() {
    let stdout = served(
        "<?php\n\
         set_error_handler(function () { unset($GLOBALS['v']); return true; });\n\
         $v = '7'; $v .= '7'; echo '1x' + $v, \"\\n\";\n\
         $a = []; $v = '7'; $v .= '7'; echo $a . $v, \"\\n\";\n",
    );
    assert_eq!(stdout, "78\nArray77\n");
}
