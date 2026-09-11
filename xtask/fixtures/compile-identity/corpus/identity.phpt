--TEST--
A function and a call, the program the fixture dumps
--FILE--
<?php
function f($a, $b) { return $a + $b * 2; }
echo f(1, 2), "\n";
--EXPECT--
5
