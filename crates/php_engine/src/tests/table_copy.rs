//! Copying a table as php copies it, as a program meets it
//! (design §5.3): the copy `get_defined_vars()` and `$GLOBALS` hand back
//! holds the value of a reference nothing else holds, so writing to it
//! changes no variable; the `(object)` cast of an array whose keys are
//! all strings, and the `(array)` cast, `get_object_vars()` and
//! `get_mangled_object_vars()` of an object with no declared property,
//! share the table where php shares it, so `memory_get_usage()` reads
//! php's delta, and a write to either holder separates. Each engine
//! arms the leak sensor, so a copy that shares where php shares
//! leaks nothing and gives up nothing twice.

use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// `src` served by a CLI engine with the leak sensor armed: what it
/// wrote, held to `expected`, and a clean exit.
fn served(src: &str, expected: &str) {
    let engine = Engine::cli_builder().sense_leaks(LeakSensor::Armed).build();
    let served = engine.serve_capture(src, Capture::default());
    let stdout = served.stdout_text();
    assert_eq!(stdout, expected);
    assert_eq!(served.exit_code, 0, "{stdout}");
}

/// A variable passed by reference, as `sort()` takes one, and one
/// whose alias was unset each hold a reference with one owner, which
/// `zend_array_dup_element` copies as its value; a variable another
/// still aliases stays a reference in the copy, as php's does. The
/// witness beside `get_defined_vars()` holds `sort()` and `extract()`
/// to the oracle; this engine has neither.
#[test]
fn writing_to_the_defined_vars_changes_no_variable() {
    served(
        r"<?php
function by_ref(&$x) {}
function f() {
    $a = [3]; by_ref($a);
    $d = [1]; $r = &$d; unset($r);
    $kept = [1]; $alias = &$kept;
    $vars = get_defined_vars();
    $vars['a'][] = 9; $vars['d'][] = 9; $vars['kept'][] = 9;
    var_dump(isset($a[1]), isset($d[1]), isset($kept[1]));
}
f();
$g = [3]; by_ref($g);
$vars = get_defined_vars(); $vars['g'][] = 9;
$all = $GLOBALS; $all['g'][] = 9;
var_dump(isset($g[1]));
",
        "bool(false)\nbool(false)\nbool(true)\nbool(false)\n",
    );
}

/// `zend_array_dup` keeps a variable's name as the string it is, where
/// `$GLOBALS`'s `zend_proptable_to_symtable` reads a decimal one as
/// the integer.
#[test]
fn the_defined_vars_keep_a_numeric_name_a_string() {
    served(
        r"<?php
function f() { ${'1'} = 5; foreach (get_defined_vars() as $k => $_) { var_dump($k); } }
f();
${'7'} = 1;
foreach (get_defined_vars() as $k => $_) { if ($k == 7) { var_dump($k); } }
foreach ($GLOBALS as $k => $_) { if ($k == 7) { var_dump($k); } }
",
        "string(1) \"1\"\nstring(1) \"7\"\nint(7)\n",
    );
}

/// The `(object)` cast of an array whose keys are all strings takes
/// the array as the object's table, one more owner of it
/// (`zend_symtable_to_proptable`), so the cast costs the 40 bytes of
/// the object; a write to the object, an unset on it and a write to
/// the array each separate. One with an integer key is converted, a
/// reference only the array held copied as its value.
#[test]
fn the_object_cast_shares_a_string_keyed_array() {
    served(
        r#"<?php
$arr = []; for ($i = 0; $i < 100; $i++) { $arr["k$i"] = $i; }
$m = memory_get_usage(); $o = (object) $arr; echo memory_get_usage() - $m, "\n";
$o->k1 = 'o'; $arr['k2'] = 'a'; unset($o->k3);
echo $arr['k1'], ' ', $o->k2, ' ', isset($arr['k3']) ? 'kept' : 'gone', ' ', isset($o->k3) ? 'kept' : 'gone', "\n";
foreach ($o as $k => &$v) { $v = 'w'; } unset($v);
echo $arr['k5'], ' ', $o->k5, "\n";
$x = 1; $refs = ['r' => &$x, 5 => 'five']; unset($x);
$c = (object) $refs; $c->r = 2;
echo $refs['r'], ' ', $c->{'5'}, "\n";
"#,
        "40\n1 2 kept gone\n5 w\n1 five\n",
    );
}

/// The `(array)` cast, `get_mangled_object_vars()` and
/// `get_object_vars()` of an object with no declared property and the
/// standard handlers take its table as the array, one more owner of it
/// (`zend_proptable_to_symtable`), so each costs nothing; a write to
/// either holder separates. A table with a decimal name is converted,
/// a reference only the table held copied as its value.
#[test]
fn the_array_cast_shares_an_object_table() {
    served(
        r#"<?php
$s = new stdClass; $s->a = 1; $s->b = 2;
$m = memory_get_usage(); $y = (array) $s; echo memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $z = get_mangled_object_vars($s); echo memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $w = get_object_vars($s); echo memory_get_usage() - $m, "\n";
$y['a'] = 5; $s->b = 6; echo $s->a, $y['b'], $z['b'], $w['b'], "\n";
$n = new stdClass; $n->{'7'} = 1; $q = 2; $n->r = &$q; unset($q);
$v = (array) $n; $v['r'] = 3;
echo $n->r, "\n"; foreach ($v as $k => $_) { var_dump($k); break; }
"#,
        "0\n0\n0\n1222\n2\nint(7)\n",
    );
}
