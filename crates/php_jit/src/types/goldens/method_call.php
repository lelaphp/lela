<?php
// #3343's goldens, as dax-jit-bench's method_call, static_call and
// new_object drive them. `k` calls a method on an object a variable
// holds: the push in place under the class guard, the receiver with one
// owner more, its frame left in place and the receiver given up after its
// variables, as php's zend_jit_init_method_call, zend_jit_push_call_frame
// and zend_jit_leave_func compile them. `s` calls a static method and `n`
// constructs an object: each pushed through its handler, the callee held
// by a guard after it, as php's zend_jit_func_guard holds it, so the sends,
// the activation and the leave are in place. `scoped` calls a method of a
// request-scoped class, `zend_lazy_class_load`'s copy of one linked over
// an evaluated class, which its handler pushes.
class Adder {
    public $base = 7;
    public function add($v) { return $this->base + $v; }
    public static function twice($v) { return $v + $v; }
}
class Point {
    public $x;
    public $y;
    public function __construct($x, $y) { $this->x = $x; $this->y = $y; }
}
eval('class Base { public $b = 1; }');
class Child extends Base { public function get($v) { return $this->b + $v; } }
function k($n) {
    $o = new Adder();
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = $o->add($i);
    }
    return $s;
}
function s($n) {
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = Adder::twice($i);
    }
    return $s;
}
function n($n) {
    $t = 0;
    for ($i = 1; $i < $n; $i++) {
        $p = new Point($i, 2);
        $t += $p->y;
    }
    return $t;
}
function scoped($o, $n) {
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = $o->get($i);
    }
    return $s;
}
echo k(100), " ", s(100), " ", n(100), " ", scoped(new Child, 100), "\n";
