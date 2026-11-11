<?php
function f($n) { try { if ($n > 1) { throw new Exception("e$n"); } return "r$n"; } finally { echo "f$n "; } }
for ($i = 0; $i < 4; $i++) { try { echo f($i), "\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; } }
$x = match (true) { 1 > 2 => "a", "08" == "8" => "b", default => "c" };
echo $x, "\n";
$i = 0; while (true) { if (++$i > 3) { break; } if ($i % 2) { continue; } echo "w$i"; }
echo "\n";
