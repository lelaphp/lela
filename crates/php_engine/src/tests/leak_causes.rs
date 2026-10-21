//! The leaks the request leak sensor found in the engine (#2057): in
//! the constant evaluator, the handlers, the collector's destructor
//! phase, the symbol tables, `Closure::call` and a frame's release,
//! each a program served with the sensor armed
//! (`EngineBuilder::sense_leaks`): what the program wrote, and no
//! `InternalError` naming a block the heap still held after it.

use crate::engine::{Engine, LeakSensor};
use crate::test::Capture;

/// `source` served by an engine with the sensor armed: what it wrote,
/// and its exit.
fn armed(source: &str) -> (String, i32) {
    let engine = Engine::cli_builder().sense_leaks(LeakSensor::Armed).build();
    let served = engine.serve_capture(source, Capture::default());
    (served.stdout_text(), served.exit_code)
}

/// A constant expression that throws while it builds an array gives up
/// the array, whether the element that threw was a key with no room
/// past the last, a constant nothing defined or a spread, and one that
/// throws while it evaluates a `new`'s arguments gives up those it
/// evaluated before (#2060).
#[test]
fn a_constant_array_that_throws_gives_up_the_array() {
    let (stdout, exit) = armed(
        r#"<?php
function test($x = [PHP_INT_MAX => 42, 'foo']) {}
try { test(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
class D { const Z = [NOPE]; const U = [PHP_INT_MAX => 1, ...[2]]; const S = [1 => 1, 'c' . 'd' => NOPE]; }
try { D::Z; } catch (Error $e) { echo $e->getMessage(), "\n"; }
try { D::U; } catch (Error $e) { echo $e->getMessage(), "\n"; }
try { D::S; } catch (Error $e) { echo $e->getMessage(), "\n"; }
define('Q', 'x');
class P { function __construct($a, $b = null) {} }
function f($x = new P([Q], NOPE)) {}
try { f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
function g($x = new P(b: [Q], a: NOPE)) {}
try { g(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
"#,
    );
    assert_eq!(
        stdout,
        "Cannot add element to the array as the next element is already occupied\n\
         Undefined constant \"NOPE\"\n\
         Cannot add element to the array as the next element is already occupied\n\
         Undefined constant \"NOPE\"\n\
         Undefined constant \"NOPE\"\n\
         Undefined constant \"NOPE\"\n"
    );
    assert_eq!(exit, 0);
}

/// A constant expression that evaluates cleanly gives up the key it
/// evaluated once the array holds its own owner of it (#2061).
#[test]
fn a_constant_array_gives_up_the_keys_it_evaluated() {
    let (stdout, exit) = armed(
        r#"<?php
const A = "ok";
function f7($x = array('aaa' => A)) { echo $x['aaa'], "\n"; }
f7();
const T = ['fo' . 'o' => 1, 'b' => 'a' . 'r'];
foreach (T as $k => $v) { echo $k, '=', $v, "\n"; }
"#,
    );
    assert_eq!(stdout, "ok\nfoo=1\nb=ar\n");
    assert_eq!(exit, 0);
}

/// A compound assignment over a property the class's pair of handlers
/// serves, a dynamic one, one bound to another by reference, one a
/// `__get` answers by reference and one named by a variable, hands the
/// stepped value to `write_property` and gives up nothing else; the
/// variable naming it keeps its string (#2071).
#[test]
fn a_compound_assignment_through_the_handlers_hands_on_its_result() {
    let (stdout, exit) = armed(
        r#"<?php
$c = 'c'; $q = 'q';
$o = new stdClass; $o->a = 'ab' . $c; $o->b =& $o->a; $o->b .= 'x'; echo $o->a, "\n";
$o->c = 1; $o->c += 1; $o->c .= 'x'; echo $o->c, "\n";
$a = ['k' => [1 => $o]]; $a['k'][1]->a .= 'y'; echo $o->a, "\n";
#[AllowDynamicProperties]
class G { private $d = ['p' => 'abc']; function &__get($n) { return $this->d[$n]; } }
$g = new G; $g->p .= 'x'; echo $g->p, "\n";
$n = 'p' . $q; $o->$n = 'a'; $o->$n .= 'x'; unset($o, $a); echo $n, "\n";
"#,
    );
    assert_eq!(stdout, "abcx\n2x\nabcxy\nabcx\npq\n");
    assert_eq!(exit, 0);
}

/// The collector's fiber, made again after a destructor suspended the
/// one before it, is given up when the request's end destroys it, and
/// the collection at the end frees it (#2073).
#[test]
fn the_collectors_fiber_is_freed_when_the_request_ends() {
    let (stdout, exit) = armed(
        r#"<?php
class Cycle {
    public $self; public $id;
    function __construct($id) { $this->id = $id; $this->self = $this; }
    function __destruct() {
        echo "{$this->id}: start\n";
        if ($this->id === 0) { global $held; $held = Fiber::getCurrent(); Fiber::suspend(); }
        echo "{$this->id}: end\n";
    }
}
$f = new Fiber(function () { new Cycle(0); new Cycle(1); gc_collect_cycles(); $GLOBALS['held']->resume(); });
$f->start();
"#,
    );
    assert_eq!(stdout, "1: start\n1: end\n0: start\n0: end\n");
    assert_eq!(exit, 0);
}

/// A generator's frame given a symbol table by a variable named by a
/// variable gives the table up when it is closed, so a generator held
/// only there is collected, and the table follows the frame to wherever
/// it is resumed; a frame popping at the place another stack's frame
/// stands frees none of that frame's table (#2073).
#[test]
fn a_frames_symbol_table_is_its_own_wherever_it_stands() {
    let (stdout, exit) = armed(
        r#"<?php
function gen() { $n = 'a'; $$n = yield; $m = 'n'; yield; echo $a === null ? 'gone' : 'held', ' ', $$m, "\n"; yield; }
$g = gen(); $g->send($g); unset($g);
echo gc_collect_cycles(), "\n";
function deep($x) { $y = 'y'; $GLOBALS['h']->next(); }
$h = gen(); $h->send(null); deep(1);
function other() { $q = 'q'; $$q = 1; }
$k = gen(); $k->send(null); other(); $k->next();
foreach (gen() as $v) {}
function inFiber() { $n = 'a'; $$n = 'A'; $m = 'n'; Fiber::suspend(); echo $a, ' ', $$m, "\n"; }
function inFiber2() { inFiber(); }
$f = new Fiber('inFiber'); $f->start();
$e = new Fiber('inFiber2'); $e->start();
function other2() { other(); }
other(); other2();
$f->resume(); $e->resume();
"#,
    );
    assert_eq!(stdout, "1\ngone a\ngone a\ngone a\nA a\nA a\n");
    assert_eq!(exit, 0);
}

/// An object whose destructor ran a collection while the store's walk
/// held it is a possible root again after the destructor, so the
/// collection at the request's end frees its cycle (#2073).
#[test]
fn an_object_served_by_the_walk_is_a_possible_root_again() {
    let (stdout, exit) = armed(
        r#"<?php
class A { public $data = []; function __destruct() { gc_collect_cycles(); }
    function b() { $this->data[] = new B($this); $this->data[] = new B($this); return $this->data[0]; } }
class B { function __construct(public $a) {} function __destruct() {} }
$a = new A; $b = $a->b(); unset($b, $a);
echo "done\n";
"#,
    );
    assert_eq!(stdout, "done\n");
    assert_eq!(exit, 0);
}

/// `Closure::call` of a closure returning by reference gives up the
/// cell it handed back for the value the cell holds (#2074).
#[test]
fn closure_call_unwraps_the_reference_it_was_returned() {
    let (stdout, exit) = armed(
        r#"<?php
$closure = function &($x) { return $x; };
echo $closure->call(new class {}, 42), "\n";
class K { private $v = 'v'; }
$get = function &() { return $this->v; };
$v = $get->call(new K); echo $v, "\n";
"#,
    );
    assert_eq!(stdout, "42\nv\n");
    assert_eq!(exit, 0);
}

/// An extra argument a frame gives up is a possible root, so a cycle a
/// collection in the callee found reachable through it is collected at
/// the request's end (#2074).
#[test]
fn an_extra_argument_given_up_is_a_possible_root() {
    let (stdout, exit) = armed(
        r#"<?php
function &foo() { $a = []; $a[] =& $a; return $a; }
function bar() { gc_collect_cycles(); }
bar(foo());
function baz() { gc_collect_cycles(); }
$o = new stdClass; $o->self = $o; baz($o); unset($o);
echo "ok\n";
"#,
    );
    assert_eq!(stdout, "ok\n");
    assert_eq!(exit, 0);
}

/// `+=` through a property the class's handlers serve, whose right
/// operand's destructor throws as the operand is freed, gives up the
/// union it wrote back, as `+` and `+=` on a variable do (#2083).
#[test]
fn an_array_union_written_back_through_the_handlers_is_given_up_when_a_destructor_throws() {
    let (stdout, exit) = armed(
        r#"<?php
try { var_dump([0] + [new class { function __destruct() { throw new Exception; } }]); } catch (Exception $e) { print "caught 2\n"; }
try { $foo = [0]; var_dump($foo += [new class { function __destruct() { throw new Exception; } }]); } catch (Exception $e) { print "caught 3\n"; }
try { $foo = (object)["foo" => [0]]; var_dump($foo->foo += [new class { function __destruct() { throw new Exception; } }]); } catch (Exception $e) { print "caught 4\n"; }
try { $foo = new class { function __get($x) { return [0]; } function __set($x, $y) {} };
    var_dump($foo->foo += [new class { function __destruct() { throw new Exception; } }]); } catch (Exception $e) { print "caught 5\n"; }
try { $foo = new class { public $bar = [0]; function &__get($x) { return $this->bar; } };
    var_dump($foo->foo += [new class { function __destruct() { throw new Exception; } }]); } catch (Exception $e) { print "caught 6\n"; }
"#,
    );
    assert_eq!(
        stdout,
        "caught 2\ncaught 3\ncaught 4\ncaught 5\n\nDeprecated: Creation of dynamic property \
         class@anonymous::$foo is deprecated in Standard input code on line 8\ncaught 6\n"
    );
    assert_eq!(exit, 0);
}
