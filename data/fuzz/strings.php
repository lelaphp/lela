<?php
$s = "abcdef";
$s[2] = "Z";
$s[8] = "!";
echo $s, "|", strlen($s), "\n";
echo str_repeat("ab", 3), substr("hello", -3, 2), ucfirst("php"), "\n";
printf("%05.2f %x %b %'*8s %-4d|\n", 3.14159, 255, 5, "pad", 7);
echo sprintf("%s", 1.0), " ", 7 / 2, " ", intdiv(7, 2), " ", 7 % -3, " ", 2 ** 10, "\n";
echo "1" + "1", " ", "1.5" + 1, " ", 0.1 + 0.2, "\n";
