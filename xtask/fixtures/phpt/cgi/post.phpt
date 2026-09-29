--TEST--
A --POST-- test runs through the CGI host, its query, cookie and form described
--GET--
q=1
--COOKIE--
c=v
--POST--
a=Hello+World
--FILE--
<?php
header('X-Fixture: 1');
echo $_POST['a'], '|', $_GET['q'], '|', $_COOKIE['c'], '|', php_sapi_name(), "\n";
?>
--EXPECTHEADERS--
X-Fixture: 1
--EXPECT--
Hello World|1|v|cgi-fcgi
