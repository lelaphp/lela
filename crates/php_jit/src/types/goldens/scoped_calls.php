<?php
// #3558's and #3557's goldens. `Child` is `zend_lazy_class_load`'s copy of
// a class linked over one the request evaluated, so its number is the
// request's own, while the methods it inherits from `Getter` are
// published. `scoped` calls such a method on a variable's object, pushed
// in place under the guard on the pair the opline's runtime-cache slot
// holds, its class the receiver's, as php's zend_jit_init_method_call
// compares `CACHED_PTR(opline->result.num)` with `obj->ce` for a class
// that is not immutable. `statics` calls a static method through the
// copy's name, pushed in place under the same slot's pair, its called
// scope the class the pair names, as php's
// zend_jit_init_static_method_call reads it from the slot.
class Getter {
    public $b = 1;
    public function get($v) { return $this->b + $v; }
    public static function twice($v) { return $v + $v; }
}
eval('class Base extends Getter { }');
class Child extends Base { }
function scoped($o, $n) {
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
echo scoped(new Child, 100), " ", statics(100), "\n";
