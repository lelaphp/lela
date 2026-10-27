<?php
// A frameless call in a loop, as a hot loop calls in_array or substr:
// class_exists and property_exists, Core's frameless natives, over a
// constant, a variable and a temporary.
function k($o, $n) {
    $c = 0;
    for ($i = 0; $i < $n; $i++) {
        $c += class_exists("stdClass") + property_exists($o, "p" . ($i & 1));
    }
    return $c;
}
class P { public $p0 = 1; }
echo k(new P, 100), "\n";
