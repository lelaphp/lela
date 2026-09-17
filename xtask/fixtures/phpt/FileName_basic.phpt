--TEST--
__FILE__ is the absolute path of the test's program, as upstream's runner hands it
--FILE--
<?php
var_dump(__FILE__);
--EXPECTF--
string(%d) "%sFileName_basic.php"
