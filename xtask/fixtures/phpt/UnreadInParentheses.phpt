--TEST--
An expression this compiler cannot read inside parentheses is one it has not read yet, not a failure
--FILE--
<?php
class C { public static $x = 1; }
$p = 'x';
$a = (C::$$p);
var_dump($a);
--EXPECT--
int(1)
