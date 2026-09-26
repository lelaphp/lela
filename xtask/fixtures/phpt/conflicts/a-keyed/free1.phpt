--TEST--
Holds no key
--FILE--
<?php
$start = microtime(true);
usleep(300000);
file_put_contents(__FILE__ . '.span', $start . ' ' . microtime(true));
echo "ok\n";
--EXPECT--
ok
