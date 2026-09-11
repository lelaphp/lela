--TEST--
A program php cannot compile, which dumps no op array
--FILE--
<?php
function f($a {
--EXPECT--
Parse error
