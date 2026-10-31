<?php
// A call passed fewer arguments than its callee has parameters, made in
// place with each default copied into its parameter, as php's trace
// compiles zend_jit_recv_init past the arguments its frame knows were
// passed (#3340). `k` calls `f` with a long, a double and an interned
// string left to their defaults; `c` calls `g`, whose default names a
// constant, which its call kernel serves.
const STEP = 3;
function f($a, $b = 2, $c = 1.5, $d = "s") { return $a + $b + $c + strlen($d); }
function g($a, $b = STEP) { return $a + $b; }
function k($n) {
    $s = 0.0;
    for ($i = 0; $i < $n; $i++) {
        $s += f($i);
    }
    return $s;
}
function c($n) {
    $s = 0;
    for ($i = 0; $i < $n; $i++) {
        $s += g($i);
    }
    return $s;
}
echo k(100), " ", c(100), "\n";
