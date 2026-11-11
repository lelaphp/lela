<?php
$values = [0, 1, -1, "0", "", "a", "1e1", "10", " 1", null, true, false, [], 1.5];
foreach ($values as $x) {
    $row = "";
    foreach ($values as $y) { $row .= ($x == $y ? "=" : ".") . ($x <=> $y); }
    echo $row, "\n";
}
echo var_export(0 == "a", true), var_export("1" == "01", true), var_export(100 == "1e2", true), "\n";
