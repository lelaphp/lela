<?php
[$unused, $profile] = $argv;
putenv('MAGIC');
set_error_handler(function($level, $text) {
    // Paths are relative; the stream path is expanded by finfo itself.
    echo json_encode(['diagnostic', str_replace(getcwd(), '<cwd>', $text)]), "\n";
    return true;
});
function rule($label) { return "0 string GIF89a $label\n!:mime application/x-$label\n"; }
file_put_contents('custom', rule('first'));
function measure($path) {
    $start = memory_get_usage();
    $f = new finfo(FILEINFO_NONE, $path);
    $opened = memory_get_usage() - $start;
    $answer = $f->buffer('GIF89a');
    $used = memory_get_usage() - $start;
    unset($answer);
    $retained = memory_get_usage() - $start;
    $f->buffer('');
    $reset = memory_get_usage() - $start;
    unset($f);
    echo json_encode(['storage', $path, $opened, $used, $retained, $reset, memory_get_usage() - $start]), "\n";
}
if ($profile === 'profiles') {
    measure(null); measure('custom'); measure('native.mgc'); measure('swapped.mgc');
    // Built-in bytes are shared, but 32 sessions own real independent state.
    $start = memory_get_usage(); $all = [];
    for ($i = 0; $i < 32; $i++) $all[] = new finfo;
    echo json_encode(['sessions', memory_get_usage() - $start]), "\n";
    unset($all); echo json_encode(['sessions-drop', memory_get_usage() - $start]), "\n";
} elseif ($profile === 'snapshot') {
    putenv('MAGIC=custom'); $old = new finfo;
    file_put_contents('custom', rule('second')); $new = new finfo;
    echo json_encode(['snapshot', $old->buffer('GIF89a'), $new->buffer('GIF89a')]), "\n";
    putenv('MAGIC=native.mgc'); $third = new finfo;
    echo json_encode(['load-time-MAGIC', $third->buffer('GIF89a')]), "\n";
    $old->__construct();
    echo json_encode(['reconstructed', $old->buffer('GIF89a')]), "\n";
} elseif ($profile === 'failure') {
    foreach (['absent', 'bad-version.mgc', 'bad-count.mgc', 'truncated.mgc', 'bad-regex', 'bad-type'] as $path) {
        $start = memory_get_usage();
        try { $f = new finfo(0, $path); echo json_encode(['loaded', $path, $f->buffer('GIF89a')]), "\n"; unset($f); }
        catch (Throwable $e) {
            $message = str_replace(getcwd(), '<cwd>', $e->getMessage());
            // Malformed compiled data can appear literally in the diagnostic.
            // Keep exact bytes as well as a readable JSON rendering.
            echo json_encode(['caught', $path, $e::class, $message, bin2hex($message)], JSON_INVALID_UTF8_SUBSTITUTE), "\n";
            unset($e, $message);
        }
        echo json_encode(['failure-drop', $path, memory_get_usage() - $start]), "\n";
    }
    // Failure after a valid database in MAGIC is not all-or-nothing.
    putenv('MAGIC=custom:absent'); $f = finfo_open();
    echo json_encode(['partial-load', $f === false ? false : $f->buffer('GIF89a')]), "\n";
} elseif ($profile === 'limit') {
    echo "before-open\n";
    $f = new finfo(0, 'large.mgc');
    echo "after-open\n";
} elseif ($profile === 'retained') {
    $f = new finfo(0, 'long-output'); $start = memory_get_usage();
    $out = $f->buffer('GIF89a');
    echo json_encode(['output', strlen($out), memory_get_usage() - $start]), "\n";
    unset($out); echo json_encode(['retained', memory_get_usage() - $start]), "\n";
    $f->buffer(''); echo json_encode(['reset', memory_get_usage() - $start]), "\n";
    unset($f); echo json_encode(['drop', memory_get_usage() - $start]), "\n";
}
