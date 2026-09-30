--TEST--
Shared: the driver's environment reaches the program, written in the driver's directory
--SKIPIF--
<?php if (getenv('REDIR_TEST_DIR') === false) die('skip no driver'); ?>
--FILE--
<?php
echo getenv('FIXTURE_DSN'), "\n";
echo getenv('REDIR_TEST_DIR') === realpath(__DIR__ . '/../../shared/tests') . '/' ? "the set's directory\n" : "another directory\n";
echo basename(dirname(__DIR__)), "\n";
?>
--EXPECT--
fixture::memory:
the set's directory
driver
