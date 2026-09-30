--TEST--
stdin is an empty pipe where the test has no --STDIN--
--FILE--
<?php
var_dump((fstat(STDIN)['mode'] & 0170000) === 0010000);
var_dump(@fseek(STDIN, 0));
var_dump(stream_get_contents(STDIN));
?>
--EXPECT--
bool(true)
int(-1)
string(0) ""
