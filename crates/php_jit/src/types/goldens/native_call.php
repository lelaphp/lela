<?php
// A native's call in a loop, as nbody's advance calls sqrt: strcmp, a Core
// native an engine with no extension has, its two arguments sent by value.
function k($a, $n) {
    $c = 0;
    for ($i = 0; $i < $n; $i++) {
        $c += strcmp($a, "m");
    }
    return $c;
}
echo k("z", 100), "\n";
