--TEST--
a SKIPIF reads a pipe on stdin too
--SKIPIF--
<?php
if ((fstat(STDIN)['mode'] & 0170000) !== 0010000) die("skip stdin is no pipe");
?>
--FILE--
<?php
echo "ran\n";
?>
--EXPECT--
ran
