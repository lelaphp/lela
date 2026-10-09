<?php
// micro.php's ucall kernel with nothing timed in the bytes: 1M calls of a user closure.
$s = 'abc'; $x = 0;
$f = function ($a) { return $a; };
for ($i = 0; $i < 1000000; $i++) { $x = $f($s); }
echo $x, "\n";
