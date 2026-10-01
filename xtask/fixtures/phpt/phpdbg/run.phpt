--TEST--
A --PHPDBG-- test runs through the debugger, its SKIPIF beside it and its ARGS after --
--SKIPIF--
<?php
if (PHP_SAPI !== 'phpdbg') die('skip the section ran outside the debugger');
?>
--PHPDBG--
r
q
--ARGS--
1 2
--FILE--
<?php
echo PHP_SAPI, ' ', implode(' ', array_slice($argv, 1)), "\n";
--EXPECTF--
[Successful compilation of %s]
prompt> phpdbg -- 1 2
[Script ended normally]
prompt>
