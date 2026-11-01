--TEST--
A --PHPDBG-- test, which runs only through phpdbg
--PHPDBG--
r
q
--FILE--
<?php echo PHP_SAPI, "\n";
--EXPECTF--
%Aphpdbg%A
