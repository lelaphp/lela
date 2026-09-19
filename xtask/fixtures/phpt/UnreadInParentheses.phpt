--TEST--
An expression this compiler cannot read inside parentheses is one it has not read yet, not a failure
--FILE--
<?php
$a = (print 1);
var_dump($a);
--EXPECT--
1int(1)
