<?php
// #3560's golden. `t` calls a method on the object `new` leaves in a
// temporary, `(new Adder)->add($i)`: pushed in place under the class
// guard, the call taking the temporary's owner over with
// ZEND_CALL_RELEASE_THIS, as php's zend_jit_init_method_call and
// zend_jit_push_call_frame compile it, and the receiver given up as its
// frame leaves.
class Adder {
    public $base = 7;
    public function add($v) { return $this->base + $v; }
}
function t($n) {
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = (new Adder)->add($i);
    }
    return $s;
}
echo t(100), "\n";
