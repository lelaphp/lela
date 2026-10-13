<?php
// #2831 actual PHP allocation observation. The callback/input exist before baseline.
function allocation($name, $operation) {
    $warm = $operation(); unset($warm);
    memory_reset_peak_usage();
    $before = memory_get_usage();
    $out = $operation();
    $held = memory_get_usage() - $before;
    $peak = memory_get_peak_usage() - $before;
    $length = strlen($out);
    unset($out);
    $released = memory_get_usage() - $before;
    echo "$name:$length:$held:$peak:$released\n";
}
mb_internal_encoding('UTF-8');
mb_substitute_character('none');
$input = str_repeat("\xff", 2050);
allocation('convert-empty-retained', fn() => mb_convert_encoding($input, 'UTF-8', 'UTF-8'));
allocation('convert-empty-fresh', fn() => mb_convert_encoding('', 'UTF-8', 'UTF-8'));
allocation('convert-one-fresh', fn() => mb_convert_encoding('a', 'UTF-8', 'UTF-8'));
$latin = str_repeat("\xe9", 2050);
allocation('convert-growth', fn() => mb_convert_encoding($latin, 'UTF-8', 'ISO-8859-1'));
$ascii = str_repeat('a', 2050);
allocation('cut-fresh', fn() => mb_strcut($ascii, 0, null, 'ASCII'));
allocation('cut-empty-interned', fn() => mb_strcut($ascii, 0, 0, 'ASCII'));
allocation('cut-one-interned', fn() => mb_strcut($ascii, 0, 1, 'ASCII'));
allocation('substr-whole-fresh', fn() => mb_substr($ascii, 0, null, 'ASCII'));
mb_http_output('pass');
allocation('http-input-reuse', fn() => mb_output_handler($ascii, 0));
allocation('mime-ascii-reuse', fn() => mb_encode_mimeheader($ascii, 'UTF-8', 'B'));
allocation('mime-speculation', fn() => mb_encode_mimeheader(str_repeat('あ', 100), 'JIS', 'B', "\r\n", 12));
allocation('cut-stateful', fn() => mb_strcut("\x1b\x24\x42".str_repeat("\x30\x21", 100)."\x1b\x28\x42", 3, 30, 'JIS'));
