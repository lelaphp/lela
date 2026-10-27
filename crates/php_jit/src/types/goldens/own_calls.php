<?php
// #3561's goldens. `Child` is `zend_lazy_class_load`'s copy of a class
// linked over one the request evaluated, and `Suit` an enum, so each is
// the request's own and so are the methods each declares, numbered by the
// request for itself. `owns`, `statics` and `cases` call such a method on
// a variable's object, through the copy's name and on an enum's case, each
// pushed in place under the guard on the pair the opline's runtime-cache
// slot holds, the callee held by its record rather than its number, as
// php's zend_jit_func_guard holds a function that is not immutable by its
// opcodes.
class Getter {
    public $b = 1;
}
eval('class Base extends Getter { }');
class Child extends Base {
    public function get($v) { return $this->b + $v; }
    public static function twice($v) { return $v + $v; }
}
enum Suit {
    case Hearts;
    public function weight($v) { return $v * 2; }
}
function owns($o, $n) {
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = $o->get($i);
    }
    return $s;
}
function statics($n) {
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = Child::twice($i);
    }
    return $s;
}
function cases($c, $n) {
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = $c->weight($i);
    }
    return $s;
}
echo owns(new Child, 100), " ", statics(100), " ", cases(Suit::Hearts, 100), "\n";
