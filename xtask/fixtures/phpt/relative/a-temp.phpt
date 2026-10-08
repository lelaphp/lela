--TEST--
Makes and removes a directory relative to the root it runs from
--FILE--
<?php
$start = microtime(true);
touch(__FILE__ . '.here');
for ($i = 0; $i < 1000 && !file_exists(__DIR__ . '/c-free.php.here'); $i++) usleep(10000);
var_dump(mkdir('relative_temp'));
usleep(100000);
var_dump(rmdir('relative_temp'));
file_put_contents(__FILE__ . '.span', $start . ' ' . microtime(true));
--EXPECT--
bool(true)
bool(true)
