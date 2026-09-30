--TEST--
stdout is a pipe, which php://stdout cannot seek
--FILE--
<?php
var_dump((fstat(STDOUT)['mode'] & 0170000) === 0010000);
$fd = fopen("php://stdout", "a");
var_dump(@fseek($fd, 1024 * 1024, SEEK_SET));
?>
--EXPECT--
bool(true)
int(-1)
