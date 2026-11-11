<?php
$n = 1;
$by_value = function ($x) use ($n) { return $x + $n; };
$by_ref = function ($x) use (&$n) { return $x + $n; };
$arrow = fn($x) => $x * $n;
$n = 10;
echo $by_value(1), " ", $by_ref(1), " ", $arrow(2), "\n";
function counter() { static $c = 0; return ++$c; }
counter(); counter();
echo counter(), "\n";
echo implode(",", array_map(fn($v) => $v * 2, array_filter([1, 2, 3, 4], fn($v) => $v % 2 === 0))), "\n";
$list = [3, 1, 2];
usort($list, fn($a, $b) => $b <=> $a);
echo implode(",", $list), "\n";
