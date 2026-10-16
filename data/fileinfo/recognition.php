<?php
putenv('MAGIC');
set_error_handler(function($level, $text) {
    echo json_encode(['diagnostic', str_replace(getcwd(), '<cwd>', $text)]), "\n"; return true;
});
$f = new finfo(FILEINFO_MIME);
foreach ([
    'ascii' => "plain text\n", 'utf8' => "h\xc3\xa9llo\n", 'utf8-bad' => "a\xc0\xaf\n",
    'utf16le' => "\xff\xfeh\0i\0\n\0", 'utf16be' => "\xfe\xff\0h\0i\0\n",
    'utf32le' => "\xff\xfe\0\0h\0\0\0\n\0\0\0", 'utf7' => "+/v8-h+AOQ-llo\n",
    'latin1' => "h\xe9llo\n", 'ebcdic' => hex2bin('c88593939615869699938415'),
    'json' => '{"a":[1,true]}', 'json-bad' => '{"a":', 'csv' => "a,b,c\n1,2,3\n",
    'cdf-truncated' => hex2bin('d0cf11e0a1b11ae1'), 'der-truncated' => hex2bin('3082ffff'),
    'gzip' => hex2bin('1f8b0800000000000003'), 'nul' => "a\0b", 'empty' => '',
] as $name => $bytes) echo json_encode([$name, $f->buffer($bytes)]), "\n";
foreach (['captures', 'continuations', 'tail', 'regex-limit'] as $db) {
    $f = new finfo(0, $db);
    foreach (['GIF89a', 'xGIF89a', str_repeat('a', 300) . '!'] as $bytes) {
        ini_set('pcre.backtrack_limit', $db === 'regex-limit' ? '1' : '1000000');
        $out = $f->buffer($bytes);
        echo json_encode(['rule', $db, strlen($bytes), $out, preg_last_error()]), "\n";
    }
}
$captures = [];
preg_match('~(G)(I)(F)(89)(a)~m', 'xGIF89a', $captures, PREG_OFFSET_CAPTURE);
echo json_encode(['PHP-captures', $captures, preg_last_error()]), "\n";
foreach (['C', 'en_US.UTF-8'] as $locale) {
    echo json_encode(['locale', $locale, setlocale(LC_CTYPE, $locale)]), "\n";
    $f = new finfo(0, 'captures'); echo json_encode(['locale-match', $f->buffer('GIF89a'), preg_last_error()]), "\n";
}
// Prefix/tail reads and CDF descriptor acquisition remain distinct paths.
$f = new finfo(0, 'tail');
echo json_encode(['tail-file', $f->file('tail-input')]), "\n";
echo json_encode(['tail-buffer', $f->buffer(file_get_contents('tail-input'))]), "\n";
$f = new finfo(FILEINFO_MIME_TYPE);
echo json_encode(['cdf-file', $f->file('cdf-input')]), "\n";
