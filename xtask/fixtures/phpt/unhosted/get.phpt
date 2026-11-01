--TEST--
A CGI-shaped test, which runs only through php-cgi
--GET--
q=1
--FILE--
<?php echo $_GET['q'], "\n";
--EXPECT--
1
