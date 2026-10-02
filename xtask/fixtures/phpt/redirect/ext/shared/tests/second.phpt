--TEST--
Shared: a second test of the set, so the driver's run judges more tests than its directory holds files
--SKIPIF--
<?php if (getenv('REDIR_TEST_DIR') === false) die('skip no driver'); ?>
--FILE--
<?php
echo getenv('FIXTURE_DSN'), "\n";
?>
--EXPECT--
fixture::memory:
