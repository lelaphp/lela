<?php
// Bounded #2826 observations, not a bench. Use the verified oracle with -n.
// '-' encodes an empty field without trailing whitespace; 'false' remains distinct.
function row(...$fields) {
    echo implode("\t", array_map(fn($field) => is_bool($field) ? (int) $field : ($field === '' ? '-' : $field), $fields)), "\n";
}
if (($argv[2] ?? '') === 'limit') {
    ini_set('memory_limit', '2M');
    $contexts = [];
    for ($i = 0; $i < 1024; ++$i) {
        $contexts[] = inflate_init(-15);
        row('limit-init', $i, memory_get_usage(), memory_get_peak_usage());
    }
    throw new RuntimeException('Expected request limit was not reached');
}
$text = str_repeat("the quick brown fox jumps over the lazy dog\n", 32);
$random = '';
$word = 0x12345678;
for ($i = 0; $i < 512; ++$i) {
    $word = ($word * 1664525 + 1013904223) & 0xffffffff;
    $random .= chr($word >> 24);
}
$inputs = ['empty' => '', 'text' => $text, 'random' => $random];
foreach ($inputs as $name => $input) {
    row('input', $name, bin2hex($input));
    foreach ([-15, 15, 31] as $wrapper) {
        foreach ([0, 1, 6, 9] as $level) {
            foreach (range(0, 4) as $strategy) {
                $c = deflate_init($wrapper, ['level' => $level, 'strategy' => $strategy]);
                row('deflate', $name, $wrapper, $level, $strategy,
                    bin2hex(deflate_add($c, $input, ZLIB_FINISH)));
            }
        }
    }
}
foreach ([-15, 15, 31] as $wrapper) {
    foreach ([8, 9, 15] as $window) {
        foreach ([1, 8, 9] as $memory) {
            $warnings = [];
            set_error_handler(function($n, $s) use (&$warnings) { $warnings[] = $s; });
            $c = deflate_init($wrapper, ['window' => $window, 'memory' => $memory]);
            restore_error_handler();
            row('parameters', $wrapper, $window, $memory,
                $c === false ? 'false' : bin2hex(deflate_add($c, $text, ZLIB_FINISH)),
                bin2hex(implode("\n", $warnings)));
        }
    }
    foreach (range(0, 5) as $flush) {
        $c = deflate_init($wrapper);
        foreach ([substr($text, 0, 7), substr($text, 7), 'again'] as $i => $chunk) {
            row('flush', $wrapper, $flush, $i, bin2hex($chunk),
                bin2hex(deflate_add($c, $chunk, $i === 1 ? ZLIB_FINISH : $flush)));
        }
    }
}
$dictionary = 'the quick brown fox ';
foreach ([-15, 15] as $wrapper) {
    $c = deflate_init($wrapper, ['dictionary' => $dictionary]);
    $bytes = deflate_add($c, $text, ZLIB_FINISH);
    row('dictionary', $wrapper, bin2hex($dictionary), bin2hex($bytes));
    foreach (['none' => null, 'right' => $dictionary, 'wrong' => 'wrong'] as $name => $dict) {
        $c = inflate_init($wrapper, $dict === null ? [] : ['dictionary' => $dict]);
        $warnings = [];
        set_error_handler(function($n, $s) use (&$warnings) { $warnings[] = $s; });
        $out = inflate_add($c, $bytes, ZLIB_FINISH);
        restore_error_handler();
        row('inflate-dictionary', $wrapper, $name, $out === false ? 'false' : bin2hex($out),
            inflate_get_status($c), inflate_get_read_len($c), bin2hex(implode("\n", $warnings)));
    }
}
$encoded = gzcompress($text);
foreach (['valid' => $encoded, 'truncated' => substr($encoded, 0, -3),
          'corrupt' => substr_replace($encoded, "\0", -1), 'invalid' => 'not zlib'] as $name => $bytes) {
    $c = inflate_init(15);
    foreach (str_split($bytes, 7) as $i => $chunk) {
        set_error_handler(fn() => true);
        $out = inflate_add($c, $chunk, ZLIB_SYNC_FLUSH);
        restore_error_handler();
        row('inflate-step', $name, $i, bin2hex($chunk), $out === false ? 'false' : bin2hex($out),
            inflate_get_status($c), inflate_get_read_len($c));
    }
}
// Actual PHP-accounted storage, not a simulated request charge.
unset($c, $out);
foreach ([8, 15] as $window) {
    gc_collect_cycles();
    $before = memory_get_usage();
    $c = inflate_init(-15, ['window' => $window]);
    $init = memory_get_usage();
    set_error_handler(fn() => true);
    $out = inflate_add($c, substr(gzdeflate($text), 0, 7), ZLIB_SYNC_FLUSH);
    restore_error_handler();
    $step = memory_get_usage();
    unset($out, $c);
    row('memory', $window, $init - $before, $step - $init, memory_get_usage() - $before);
}
// Files are isolated non-shipped evidence, under the collector's approved scratch.
if (!isset($argv[1]) || !is_dir($argv[1])) {
    throw new RuntimeException('Pass an existing evidence scratch directory');
}
$path = $argv[1] . '/compression.bin';
foreach (['transparent' => $text, 'concatenated' => gzencode('first') . gzencode('second'),
          'trailing' => gzencode('first') . 'not gzip'] as $name => $bytes) {
    file_put_contents($path, $bytes);
    $c = gzopen($path, 'rb');
    $head = gzread($c, 3);
    $position = gztell($c);
    $seek = gzseek($c, 0);
    $rest = stream_get_contents($c);
    row('gzip-file', $name, bin2hex($head), $position, $seek, bin2hex($rest), gzeof($c), gzclose($c));
}
unlink($path);
if (class_exists('ZipArchive')) {
    foreach ($inputs as $name => $input) {
        foreach ([0, 1, 6, 9] as $level) {
            $zip = new ZipArchive;
            $zip->open($path, ZipArchive::CREATE | ZipArchive::OVERWRITE);
            $zip->addFromString('entry', $input);
            $zip->setCompressionName('entry', ZipArchive::CM_DEFLATE, $level);
            $zip->close();
            $zip->open($path);
            $stat = $zip->statIndex(0);
            // getFromIndex special-cases zero original size, even when FL_COMPRESSED
            // was requested. Extract the physical entry from this bounded one-entry
            // archive instead; preserve the distinct API observation as well.
            $archive = file_get_contents($path);
            $header = unpack('Vsignature/vversion/vflags/vmethod/vtime/vdate/Vcrc/Vsize/Voriginal/vname/vextra', substr($archive, 0, 30));
            if ($header['signature'] !== 0x04034b50) {
                throw new RuntimeException('Expected a ZIP local header');
            }
            $packed = substr($archive, 30 + $header['name'] + $header['extra'], $stat['comp_size']);
            if (strlen($packed) !== $stat['comp_size']) {
                throw new RuntimeException('Truncated physical ZIP entry');
            }
            $api = $zip->getFromIndex(0, 0, ZipArchive::FL_COMPRESSED);
            row('zip-entry', $name, $level, $stat['comp_method'], bin2hex($packed),
                $stat['comp_size'], $api === false ? 'false' : 'string', $api === false ? '' : bin2hex($api));
            $zip->close();
            unlink($path);
        }
    }
} else {
    row('unavailable', 'ZIP extension');
}
