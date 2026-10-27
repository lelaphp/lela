<?php
// A frameless call in a loop, as a hot loop calls in_array or substr:
// class_exists and property_exists, Core's frameless natives, over a
// constant, a variable's reference, a temporary and a long held in a
// register.
function k($o, $n) {
    $c = 0;
    $p = &$o;
    for ($i = 0; $i < $n; $i++) {
        $c += class_exists("stdClass", $i & 1) + property_exists($p, "p" . ($i & 1));
    }
    return $c;
}
class P { public $p0 = 1; }
echo k(new P, 100), "\n";
