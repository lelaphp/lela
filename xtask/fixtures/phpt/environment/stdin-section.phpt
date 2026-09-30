--TEST--
stdin is a pipe holding the --STDIN-- section
--STDIN--
fed
--FILE--
<?php
var_dump((fstat(STDIN)['mode'] & 0170000) === 0010000);
var_dump(trim(stream_get_contents(STDIN)));
?>
--EXPECT--
bool(true)
string(3) "fed"
