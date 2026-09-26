--TEST--
Never ends
--FILE--
<?php
file_put_contents(__FILE__ . '.span', microtime(true) . ' 0');
while (true) {}
--EXPECT--
nothing
