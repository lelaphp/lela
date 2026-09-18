--TEST--
An expression this compiler cannot read inside parentheses is one it has not read yet, not a failure
--FILE--
<?php
$a = 1;
$b = 2;
$c = ($a and $b);
var_dump($c);
--EXPECT--
bool(true)
