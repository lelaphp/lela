--TEST--
Holds its own key, not the directory's
--CONFLICTS--
own
--FILE--
<?php
$start = microtime(true);
touch(__FILE__ . '.here');
for ($i = 0; $i < 1000 && !file_exists(__DIR__ . '/two.php.here'); $i++) usleep(10000);
usleep(300000);
file_put_contents(__FILE__ . '.span', $start . ' ' . microtime(true));
echo "ok\n";
--EXPECT--
ok
