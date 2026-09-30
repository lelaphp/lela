--TEST--
the environment is run-tests.php's: the two skips set, an empty value and the emptied CGI variables absent
--ENV--
emptyval=
--FILE--
<?php
var_dump(getenv('SKIP_ONLINE_TESTS'), getenv('SKIP_IO_CAPTURE_TESTS'));
var_dump(getenv('emptyval'), getenv('TZ'), getenv('QUERY_STRING'));
?>
--EXPECT--
string(1) "1"
string(1) "1"
bool(false)
bool(false)
bool(false)
