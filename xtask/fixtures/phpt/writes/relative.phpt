--TEST--
Leaves a file and a directory relative to the root it runs from
--FILE--
<?php
var_dump(file_put_contents('relative.txt', 'left') === 4);
var_dump(mkdir('relative_dir'));
var_dump(file_exists(getenv('TEST_PHP_SRCDIR') . '/relative.txt'));
--EXPECT--
bool(true)
bool(true)
bool(true)
