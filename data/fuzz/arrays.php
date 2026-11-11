<?php
$a = [3 => "x", "08" => "y", "8" => "z", true => "t", null => "n"];
$b = $a;
$b[] = "appended";
$b["8"] = "over";
echo count($a), " ", count($b), "\n";
foreach ($b as $k => $v) { echo gettype($k), ":", $k, "=", $v, "\n"; }
echo isset($a[8]) ? "y" : "n", isset($a["08"]) ? "y" : "n", "\n";
echo implode(",", array_keys($b)), "\n";
