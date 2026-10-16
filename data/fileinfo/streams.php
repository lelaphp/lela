<?php
[$unused, $kind, $mode] = $argv;
$events = [];
function event($name, $value = null) { global $events; $events[] = [$name, $value]; }
set_error_handler(function($level, $text) { event('diagnostic', $text); return true; });
class StreamProof {
    public $context;
    private $at = 0;
    private $seeks = 0;
    public function stream_open($path, $mode, $options, &$opened) { event('open'); return true; }
    public function stream_stat() {
        global $kind;
        event('stat');
        return $kind === 'stat-fail' ? false : ['mode' => 0100000, 'size' => 600];
    }
    public function stream_read($count) {
        global $kind;
        event('read', [$count, $this->at]);
        if ($kind === 'read-fail') return false;
        $n = $kind === 'short' ? 2 : $count;
        $text = substr(str_repeat('GIF89a', 100), $this->at, $n);
        $this->at += strlen($text); return $text;
    }
    public function stream_eof() { event('eof', $this->at); return $this->at >= 600; }
    public function stream_tell() { event('tell', $this->at); return $this->at; }
    public function stream_seek($offset, $whence) {
        global $kind;
        event('seek', [$offset, $whence]);
        $this->seeks++;
        if ($kind === 'nonseekable' || ($kind === 'rewind-fail' && $offset === 0)
            || ($kind === 'restore-fail' && $this->seeks >= 2)) return false;
        $this->at = $offset; return true;
    }
    public function stream_cast($cast) { event('cast', $cast); return false; }
    public function stream_close() {
        global $kind;
        event('close');
        if ($kind === 'close-throw') throw new Exception('close failed');
    }
}
stream_wrapper_register('proof', StreamProof::class);
try {
    if ($mode === 'file') {
        event('answer', (new finfo(FILEINFO_MIME_TYPE))->file('proof://file'));
    } else {
        if ($kind === 'plain' || $kind === 'filtered') {
            file_put_contents('plain', str_repeat('gif89a', 100));
            $s = fopen('plain', 'rb');
            if ($kind === 'filtered') stream_filter_append($s, 'string.toupper', STREAM_FILTER_READ);
        } elseif ($kind === 'socket') {
            [$s, $other] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, STREAM_IPPROTO_IP);
            fwrite($other, 'GIF89a'); fclose($other);
        } else $s = fopen('proof://file', 'rb');
        fread($s, 3);
        event('before', ftell($s));
        event('answer', mime_content_type($s));
        event('after', ftell($s));
        fclose($s);
    }
} catch (Throwable $e) { event('caught', [$e::class, $e->getMessage()]); }
echo json_encode($events), "\n";
