--TEST--
The driver's own test runs as itself beside the set
--FILE--
<?php
echo getenv('FIXTURE_DSN') === false ? "no dsn\n" : "a dsn\n";
?>
--EXPECT--
no dsn
