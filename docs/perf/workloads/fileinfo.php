<?php
// #2827 inputs, not a timing baseline or settled replacement tolerance.
// The paired instrument times the process; these bytes contain no clock.
[$unused, $profile] = $argv;
putenv('MAGIC');
$bytes = "GIF89a" . str_repeat("\0", 4090);
$database = "0 string GIF89a custom\n!:mime image/gif\n";
class FileinfoWorkloadStream {
    public $context;
    private $at = 0;
    public function stream_open($path, $mode, $options, &$opened) { return true; }
    public function stream_stat() { return ['mode' => 0100000, 'size' => 4096]; }
    public function stream_read($count) {
        global $bytes; $out = substr($bytes, $this->at, $count); $this->at += strlen($out); return $out;
    }
    public function stream_eof() { return $this->at >= 4096; }
    public function stream_cast($cast) { return false; }
    public function stream_close() {}
}
stream_wrapper_register('fileinfo-workload', FileinfoWorkloadStream::class);
// tmpfile lifetime is one arm; both inputs are identical and request-owned.
$file = tmpfile(); fwrite($file, $bytes); $path = stream_get_meta_data($file)['uri'];
$db = tmpfile(); fwrite($db, $database); $dbpath = stream_get_meta_data($db)['uri'];
$f = new finfo(FILEINFO_MIME_TYPE);
$answers = '';
for ($i = 0; $i < 256; $i++) {
    $answers .= match ($profile) {
        'buffer' => $f->buffer($bytes),
        'file' => $f->file($path),
        'wrapper' => $f->file('fileinfo-workload://input'),
        'cold' => (new finfo(FILEINFO_MIME_TYPE, $dbpath))->buffer($bytes),
        default => throw new Exception('unknown workload'),
    } . "\n";
}
echo $profile, ' ', hash('sha256', $answers), "\n";
fclose($db); fclose($file);
