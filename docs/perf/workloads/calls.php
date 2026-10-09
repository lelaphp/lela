<?php
// #1808's calls.php as its comment states it (the file itself is not kept):
// 2M user calls (DO_UCALL with a RETURN) twice, 2M DO_ICALL (intdiv),
// 2M FRAMELESS_ICALL_2 (max).
function f($a) { return $a; }
function g($a, $b) { return $a + $b; }
$x = 0;
for ($i = 0; $i < 2000000; $i++) { $x = f($i); }
for ($i = 0; $i < 2000000; $i++) { $x = g($i, 1); }
for ($i = 0; $i < 2000000; $i++) { $x = intdiv($i, 7); }
for ($i = 0; $i < 2000000; $i++) { $x = max($i, $x); }
echo $x, "\n";
