--TEST--
Makes nothing, in a batch of its own
--FILE--
<?php
$start = microtime(true);
usleep(100000);
file_put_contents(__FILE__ . '.span', $start . ' ' . microtime(true));
echo "ok\n";
--EXPECT--
ok
