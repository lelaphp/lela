--TEST--
Makes nothing, in a batch of its own
--FILE--
<?php
$start = microtime(true);
touch(__FILE__ . '.here');
for ($i = 0; $i < 1000 && !file_exists(__DIR__ . '/a-temp.php.here'); $i++) usleep(10000);
usleep(100000);
file_put_contents(__FILE__ . '.span', $start . ' ' . microtime(true));
echo "ok\n";
--EXPECT--
ok
