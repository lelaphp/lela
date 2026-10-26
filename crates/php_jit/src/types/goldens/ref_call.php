<?php
// A call as nbody's main loop calls advance: arrays sent by reference to
// typed parameters, an array and a float by value, made in place with the
// RECVs' checks inline, as php's zend_jit_recv checks what its stack map
// does not know (#3340).
function bump(array &$a, array &$b, array $m, float $d): void { $a[0] += $d; $b[0] += $m[0]; }
function k($n) {
    $x = [0.0]; $y = [0.0]; $m = [1.0];
    for ($i = 0; $i < $n; $i++) {
        bump($x, $y, $m, 0.5);
    }
    return $x[0] + $y[0];
}
echo k(100), "\n";
