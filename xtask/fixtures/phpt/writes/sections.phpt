--TEST--
Leaves a file from its SKIPIF, from its program through {PWD} and from its CLEAN
--SKIPIF--
<?php file_put_contents(__DIR__ . '/sections.skipif.txt', 'left');
--ENV--
WHERE={PWD}/sections.env.txt
--FILE--
<?php
var_dump(file_put_contents(getenv('WHERE'), 'left') === 4);
var_dump(dirname(getenv('WHERE')) === __DIR__);
var_dump(file_exists(__DIR__ . '/sections.skipif.txt'));
--CLEAN--
<?php file_put_contents(__DIR__ . '/sections.clean.txt', 'left');
--EXPECT--
bool(true)
bool(true)
bool(true)
