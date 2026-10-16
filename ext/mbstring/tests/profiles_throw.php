<?php
set_error_handler(function () { throw new Exception('reentered'); });
$input = ["\xe9" => "\xe9", new stdClass()];
try { mb_convert_encoding($input, 'UTF-8', 'ISO-8859-1'); }
catch (Exception $e) { echo $e->getMessage(), "\n"; }
restore_error_handler();
$count = 0; foreach ($input as $value) $count++;
echo $count, "\n";
