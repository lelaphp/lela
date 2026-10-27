<?php
// #3561's goldens. `selfs` calls a static method through `self`, pushed in
// place under the pair its runtime-cache slot holds, its called scope the
// calling frame's, as php's zend_jit_init_static_method_call compiles a
// call over `self` or `parent`. `named` and `parents` call a method that
// takes `$this` by a static call's syntax, through the class's name and
// through `parent`, pushed in place with the calling frame's `$this`, as
// php's zend_jit_push_this_method_call_frame passes it.
class Counter {
    public $base = 3;
    public static function twice($v) { return $v + $v; }
    public function add($v) { return $this->base + $v; }
    public static function selfs($n) {
        $s = 0;
        for ($i = 1; $i < $n; $i++) {
            $s = self::twice($i);
        }
        return $s;
    }
    public function named($n) {
        $s = 0;
        for ($i = 1; $i < $n; $i++) {
            $s = Counter::add($i);
        }
        return $s;
    }
}
class Sub extends Counter {
    public function parents($n) {
        $s = 0;
        for ($i = 1; $i < $n; $i++) {
            $s = parent::add($i);
        }
        return $s;
    }
}
echo Counter::selfs(100), " ", (new Counter)->named(100), " ", (new Sub)->parents(100), "\n";
