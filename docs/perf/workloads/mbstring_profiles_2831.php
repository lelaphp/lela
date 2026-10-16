<?php
// #2831 workload INPUT, not a timing/acceptance claim. mode and size are bar arguments.
// Run paired arms and A/A through xtask bench in CI under measurement-protocol.md.
$mode = $argv[1] ?? 'convert';
$size = $argv[2] ?? 'small';
if ($size !== 'small' && $size !== 'large') throw new Exception('unknown size');
$count = $size === 'small' ? 16 : 32768;
$iterations = $size === 'small' ? 1000 : 8;
$input = str_repeat('あ€', $count);
$checksum = 0;
mb_internal_encoding('UTF-8');
mb_substitute_character(0x3f);
for ($i = 0; $i < $iterations; $i++) {
    if ($mode === 'convert') {
        $out = mb_convert_encoding($input, 'GB18030-2022', 'UTF-8');
    } elseif ($mode === 'mime') {
        $out = mb_encode_mimeheader($input, 'JIS', 'B', "\r\n", 12);
    } elseif ($mode === 'cut') {
        $jis = mb_convert_encoding($input, 'JIS', 'UTF-8');
        $out = mb_strcut($jis, 0, strlen($jis) - 4, 'JIS');
    } elseif ($mode === 'http') {
        mb_http_output('JIS');
        $out = mb_output_handler($input, PHP_OUTPUT_HANDLER_START);
        $out .= mb_output_handler($input, PHP_OUTPUT_HANDLER_FINAL);
    } else {
        throw new Exception('unknown operation');
    }
    // Non-empty correctness bytes before any timings may be read.
    $checksum += strlen($out);
}
echo $mode, ':', $size, ':', $checksum, ':', hash('sha256', $out), "\n";
