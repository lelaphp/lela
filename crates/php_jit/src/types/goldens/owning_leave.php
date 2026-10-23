<?php
// A function's leave over variables that own something, compiled in place:
// `h`'s `$a` holds the array its caller passes, which the caller keeps too,
// and `$s` the string it builds, each freed as php's zend_jit_free_cv frees
// it before zend_jit_leave_func (#3313).
function h($a, $i) { $s = "n" . $i; return $a[0] + $i; }
function k($n) {
    $v = [1, 2]; $c = 0;
    for ($i = 0; $i < $n; $i++) {
        $c += h($v, $i);
    }
    return $c;
}
echo k(100), "\n";
