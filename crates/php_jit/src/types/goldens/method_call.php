<?php
// A method called on an object a variable holds, as dax-jit-bench's
// method_call drives it: the method's frame left in place, its receiver
// given up after its variables, as php's zend_jit_leave_func gives it up
// (#3343).
class Adder {
    public $base = 7;
    public function add($v) { return $this->base + $v; }
}
function k($n) {
    $o = new Adder();
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = $o->add($i);
    }
    return $s;
}
echo k(100), "\n";
