--TEST--
A header the test wants and the run did not write fails it, whatever the body did
--POST--
a=1
--FILE--
<?php
echo $_POST['a'];
?>
--EXPECTHEADERS--
X-Never: 1
--EXPECT--
1
