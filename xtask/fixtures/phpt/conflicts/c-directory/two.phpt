--TEST--
Holds the directory's key
--FILE--
<?php
$start = microtime(true);
touch(__FILE__ . '.here');
for ($i = 0; $i < 1000 && !file_exists(__DIR__ . '/own.php.here'); $i++) usleep(10000);
usleep(300000);
file_put_contents(__FILE__ . '.span', $start . ' ' . microtime(true));
echo "ok\n";
--EXPECT--
ok
