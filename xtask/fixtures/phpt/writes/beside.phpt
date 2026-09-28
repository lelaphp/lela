--TEST--
Leaves a file beside itself
--FILE--
<?php
var_dump(file_put_contents(__DIR__ . '/beside.txt', 'left') === 4);
var_dump(realpath(getenv('TEST_PHP_SRCDIR')) === getcwd());
var_dump(str_starts_with(__DIR__, getcwd() . '/'));
--EXPECT--
bool(true)
bool(true)
bool(true)
