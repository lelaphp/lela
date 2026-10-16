<?php
// #2826 workload proposal. No tolerance or timing verdict is selected here.
// Paired arms must use identical arguments and compare stdout inside each rep.
// MySQL here is only the source-derived payload shape, not a network acceptance test.
$kind = $argv[1] ?? 'small';
$iterations = (int) ($argv[2] ?? 1);
if (!in_array($kind, ['small', 'large', 'incompressible', 'incremental', 'gzip', 'zip', 'mysql'], true) || $iterations < 1) {
    throw new InvalidArgumentException('Select a workload and a positive iteration count');
}
$text = str_repeat("the quick brown fox jumps over the lazy dog\n", $kind === 'small' ? 4 : 24576);
$input = $text;
if ($kind === 'incompressible' || $kind === 'mysql') {
    $input = '';
    $word = 0x12345678;
    for ($i = 0; $i < ($kind === 'mysql' ? 16384 : 1048576); ++$i) {
        $word = ($word * 1664525 + 1013904223) & 0xffffffff;
        $input .= chr($word >> 24);
    }
}
$out = '';
for ($rep = 0; $rep < $iterations; ++$rep) {
    if ($kind === 'incremental') {
        $context = deflate_init(ZLIB_ENCODING_RAW);
        foreach (str_split($input, 7) as $chunk) {
            $out .= deflate_add($context, $chunk, ZLIB_SYNC_FLUSH);
        }
        $out .= deflate_add($context, '', ZLIB_FINISH);
    } elseif ($kind === 'gzip') {
        $out .= gzencode($input, 6);
    } elseif ($kind === 'zip') {
        $context = deflate_init(ZLIB_ENCODING_RAW, ['level' => 9, 'memory' => 9]);
        foreach (str_split($input, 8192) as $chunk) {
            $out .= deflate_add($context, $chunk, ZLIB_NO_FLUSH);
        }
        $out .= deflate_add($context, '', ZLIB_FINISH);
    } elseif ($kind === 'mysql') {
        $payload = "\0\0\0\0" . $input;
        $packed = gzcompress($payload);
        $out .= strlen($packed) <= strlen($payload) ? $packed : $payload;
    } else {
        $out .= gzcompress($input, 6);
    }
}
echo $kind, ':', strlen($out), ':', hash('sha256', $out), "\n";
