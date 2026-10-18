<?php
// #1301: a call-bound program for the bar #1384 holds the frame chain's
// header to: recursive user calls, then a loop of native calls.
function fib($n) { return $n < 2 ? $n : fib($n - 1) + fib($n - 2); }
$x = fib(28);
$n = 0;
for ($i = 0; $i < 3000000; $i++) { $n += intdiv($i, 7); }
echo $x, ' ', $n, "\n";
