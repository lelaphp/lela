<?php
// Oracle transaction input, not a replacement recognizer.
[$unused, $hook, $override, $action] = $argv;
$events = [];
$database = match ($hook) {
    'regex-warning' => 'lazy-bad-regex.mgc',
    'buffer-failure' => 'buffer-failure',
    default => null,
};
$f = new finfo(FILEINFO_NONE, $database);
$fired = false;
function event($name, $value = null) {
    global $events;
    $events[] = [$name, $value];
}
function callback($name) {
    global $hook, $action, $f, $fired;
    event($name);
    if ($name !== $hook || $fired) return;
    $fired = true;
    if ($action === 'replace') {
        event('replace-begin');
        $f->__construct(FILEINFO_MIME_ENCODING);
        event('replace-end');
    } elseif ($action === 'throw') {
        $f->set_flags(FILEINFO_MIME_ENCODING);
        event('set-encoding');
        throw new Exception('callback throw');
    } else {
        // Closing after classification can invalidate retained output if it
        // classifies again. Test flags here without freeing that output.
        $nested = in_array($hook, ['regex-warning', 'buffer-failure']) ? '' : 'GIF89a';
        if ($name !== 'close') event('nested-before', $f->buffer($nested));
        event('set-encoding', $f->set_flags(FILEINFO_MIME_ENCODING));
        if ($name !== 'close') event('nested-after', $f->buffer($nested));
    }
}
class ProofStream {
    public $context;
    private $at = 0;
    public function stream_open($path, $mode, $options, &$opened) {
        callback('open'); return true;
    }
    public function stream_stat() {
        callback('stat'); return ['mode' => 0100000, 'size' => 6];
    }
    public function stream_read($count) {
        global $hook;
        callback('read');
        if ($hook === 'identify-warning') return false;
        $bytes = substr('GIF89a', $this->at, $count);
        $this->at += strlen($bytes); return $bytes;
    }
    public function stream_eof() { return $this->at >= 6; }
    public function stream_cast($cast) { callback('cast'); return false; }
    public function stream_close() { callback('close'); }
}
stream_wrapper_register('proof', ProofStream::class);
set_error_handler(function($level, $text) {
    event('diagnostic', $text);
    global $hook;
    callback(in_array($hook, ['identify-warning', 'regex-warning', 'buffer-failure']) ? $hook : 'warning');
    return true;
});
try {
    if ($hook === 'deprecation') {
        restore_error_handler();
        set_error_handler(function($level, $text) {
            event('diagnostic', $text); callback('deprecation'); return true;
        });
        event('outer', $f->buffer('GIF89a', (int)$override, null));
    } elseif (in_array($hook, ['regex-warning', 'buffer-failure'])) {
        event('outer', $f->buffer('GIF89a', (int)$override));
    } else {
        event('outer', $f->file($hook === 'warning' ? 'missing://file' : 'proof://file', (int)$override));
    }
} catch (Throwable $e) { event('caught', [$e::class, $e->getMessage()]); }
event('after', $f->buffer(in_array($hook, ['regex-warning', 'buffer-failure']) ? '' : 'GIF89a'));
echo json_encode($events, JSON_INVALID_UTF8_SUBSTITUTE), "\n";
