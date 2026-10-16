<?php
// #2831 pinned-oracle input, non-shipped. Defer printing to keep headers mutable.
$rows = [];
function row($name, $bytes) { global $rows; $rows[] = [$name, $bytes, mb_get_info('illegal_chars')]; }
mb_internal_encoding('UTF-8');
mb_substitute_character(0x3f);
foreach (["\xf0\x9f", "\xc0\xaf", "\xed\xa0\x80", "\xe2\x28\xa1"] as $bytes) {
    row('bad-utf8', mb_convert_encoding($bytes, 'UTF-8', 'UTF-8'));
}
row('jis-final', mb_convert_encoding('あ', 'JIS', 'UTF-8'));
row('jis-truncated', mb_convert_encoding("\x1b\x24\x42\x30", 'UTF-8', 'JIS'));
row('gb2022-terminal', mb_convert_encoding("\x81\x30\x81", 'UTF-8', 'GB18030-2022'));
mb_substitute_character('long');
row('target-long', mb_convert_encoding('€', 'ASCII', 'UTF-8'));
mb_substitute_character(0x3f);
foreach ([0, 3] as $start) {
    foreach ([0, 1, 4, 7, 8, 9, 20, 40] as $length) {
        row('cut-jis-'.$start.'-'.$length, mb_strcut("\x1b\x24\x42\x30\x21\x30\x22\x1b\x28\x42", $start, $length, 'JIS'));
    }
}
row('cut-fixed', mb_strcut("\x00a\x00b\x00c", 1, 3, 'UTF-16BE'));
row('cut-lead', mb_strcut("\x82\xa0\x82\xa2", 1, 3, 'SJIS'));
row('cut-dedicated', mb_strcut('あいう', 1, 5, 'UTF-8'));
foreach (['B', 'Q'] as $transfer) {
    $input = ''; for ($i = 0; $i < 18; $i++) $input .= 'あ€';
    row('mime-'.$transfer, mb_encode_mimeheader($input, 'JIS', $transfer, "\r\n", 12));
}
// Decoding state remembers the BOM; each response has a new target encoder.
mb_internal_encoding('UTF-16');
mb_http_output('JIS');
$a = mb_output_handler("\xfe\xff\x30\x42", PHP_OUTPUT_HANDLER_START);
$b = mb_output_handler("\x30\x44", PHP_OUTPUT_HANDLER_FINAL);
row('http-first', $a);
row('http-fresh-encoder', $b);
mb_internal_encoding('UTF-16');
$a = mb_output_handler("\xfe\xff\x30\x42", PHP_OUTPUT_HANDLER_START);
mb_http_output('ASCII');
mb_internal_encoding('UTF-16BE');
mb_substitute_character(0x21);
$b = mb_output_handler("\x30\x44", PHP_OUTPUT_HANDLER_FINAL);
row('http-settings', $b);
mb_internal_encoding('UTF-8');
mb_http_output('JIS');
$a = mb_output_handler('あ', PHP_OUTPUT_HANDLER_START);
$b = mb_output_handler('', PHP_OUTPUT_HANDLER_FINAL);
row('http-empty-final', $a.$b);
set_error_handler(function ($severity, $message) {
    row('warning', $message);
    mb_substitute_character(0x23);
    row('nested', mb_convert_encoding("\xff", 'UTF-8', 'UTF-8'));
    return true;
});
$input = [new stdClass(), "\xff"];
$out = mb_convert_encoding($input, 'UTF-8', 'UTF-8');
row('array-after-warning', $out[1]);
restore_error_handler();
foreach ($rows as $r) { echo $r[0], ':', $r[2], "\n"; var_dump($r[1]); }
