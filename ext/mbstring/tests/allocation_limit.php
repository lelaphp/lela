<?php
// Run in an isolated oracle process; this deliberately reaches its memory fatal.
$kind = $argv[1];
mb_substitute_character('none');
if ($kind === 'initial') {
    $input = str_repeat("\xff", 8 * 1024 * 1024);
    ini_set('memory_limit', '12M');
    $output = mb_convert_encoding($input, 'UTF-8', 'UTF-8');
} elseif ($kind === 'growth') {
    $input = str_repeat("\xe9", 3 * 1024 * 1024);
    ini_set('memory_limit', '10M');
    $output = mb_convert_encoding($input, 'UTF-8', 'ISO-8859-1');
} else {
    throw new Exception('unknown allocation profile');
}
echo 'unexpected-success:', strlen($output), "\n";
