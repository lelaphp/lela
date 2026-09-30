--TEST--
A construct this compiler cannot read is one it has not read yet, not a failure
--FILE--
<?php
declare(ticks=1): var_dump(1); enddeclare;
--EXPECT--
int(1)
