<?php
[$unused, $kind] = $argv;
$events = [];
function event($name, $value = null) { global $events; $events[] = [$name, $value]; }
set_error_handler(function($level, $text) { event('diagnostic', $text); return true; });
class DescriptorProof {
    public $context;
    private $fd;
    private $prefix;
    private $at = 0;
    public function stream_open($path, $mode, $options, &$opened) {
        global $kind;
        event('open');
        $this->prefix = file_get_contents($kind === 'cdf' ? 'cdf-input' : 'tail-input', false, null, 0, 8192);
        if ($kind === 'tail-pipe') {
            [$this->fd, $write] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, STREAM_IPPROTO_IP);
            fwrite($write, 'TAIL'); fclose($write);
        } else $this->fd = fopen($kind === 'tail-short' ? 'native.mgc' : ($kind === 'cdf' ? 'cdf-input' : 'tail-input'), 'rb');
        return true;
    }
    public function stream_stat() {
        global $kind;
        event('stat');
        return ['mode' => 0100000, 'size' => filesize($kind === 'cdf' ? 'cdf-input' : 'tail-input')];
    }
    public function stream_read($count) {
        event('prefix-read', $count);
        $out = substr($this->prefix, $this->at, $count); $this->at += strlen($out); return $out;
    }
    public function stream_eof() { return false; }
    public function stream_cast($cast) {
        global $kind;
        event('cast', $cast); return $kind === 'missing' ? false : $this->fd;
    }
    public function stream_close() {
        event('descriptor-position', ftell($this->fd));
        // PHP's cached stream position does not track direct descriptor I/O.
        event('descriptor-next', bin2hex(fread($this->fd, 4)));
        fclose($this->fd); event('descriptor-closed', !is_resource($this->fd)); event('close');
    }
}
stream_wrapper_register('descriptor', DescriptorProof::class);
$f = new finfo($kind === 'cdf' ? FILEINFO_MIME_TYPE : FILEINFO_NONE,
               in_array($kind, ['cdf', 'plain']) ? null : 'tail');
event('answer', $f->file('descriptor://file'));
echo json_encode($events), "\n";
