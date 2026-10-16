<?php
putenv('MAGIC');
ini_set('pcre.jit', '0');
set_error_handler(function($level, $text) { echo json_encode(['diagnostic', $text]), "\n"; return true; });
echo "before-compiled-load\n";
$f = finfo_open(0, 'lazy-bad-regex.mgc');
echo json_encode(['loaded', $f !== false]), "\n";
echo "before-compiled-match\n";
echo json_encode(['matched', $f->buffer('GIF89a'), preg_last_error()]), "\n";
echo "after-compiled-match\n";
echo "before-text-load\n";
$f = finfo_open(0, 'bad-regex');
echo json_encode(['loaded-text', $f !== false, preg_last_error()]), "\n";
foreach (['capture-zero', 'capture-many'] as $path) {
    $f = new finfo(0, $path);
    $f->buffer('GIF89a'); // pattern cache warm before the peak observation
    memory_reset_peak_usage(); $start = memory_get_usage();
    $out = $f->buffer('GIF89a');
    echo json_encode(['capture-peak', $path, memory_get_peak_usage() - $start, $out]), "\n";
}
ini_set('pcre.recursion_limit', '1');
echo json_encode(['recursion', $f->buffer('GIF89a'), preg_last_error()]), "\n";
// PHP's all-capture operation is the existing reusable semantic seam.
ini_set('pcre.recursion_limit', '100000');
$matches = []; preg_match('~(G)(I)(F)(89)(a)~m', 'xGIF89a', $matches, PREG_OFFSET_CAPTURE);
echo json_encode(['captures', $matches]), "\n";
