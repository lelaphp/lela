--TEST--
A construct this compiler cannot read is one it has not read yet, not a failure
--FILE--
<?php
declare(encoding='UTF-8');
var_dump(1);
--EXPECT--
int(1)
