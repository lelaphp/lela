<?php
// string_munge — log/CSV line processing over a large in-memory text blob.
//
// Exercises: byte-string operations (trim/strtoupper/str_replace/substr/strpos),
// explode/implode allocation churn, string concatenation, and crc32. This is
// the test for how the engine handles the bread-and-butter of real PHP web
// code: take a chunk of text, split it into lines and fields, normalise each
// field, and stitch it back together. Lots of short-lived string allocations.
//
// Deterministic output: line count, total rebuilt length, and a crc32 checksum.

$N = 600000; // @bench-param  (number of CSV/log lines to generate and process)

// Build a big deterministic text blob: one CSV-ish line per record. No rand —
// every field is derived from the row index so output is byte-stable.
$levels = ['info', 'warn', 'error', 'debug', 'trace'];
$blob = '';
for ($i = 0; $i < $N; $i++) {
    $level = $levels[$i % 5];
    $code  = ($i * 2654435761) % 100000;
    $path  = "/api/v1/resource/" . ($i % 997) . "/item-" . ($i % 31);
    // Sprinkle in irregular whitespace so trim() has real work to do.
    $blob .= "  $i , $level ,  GET ,$path , $code , ok-$level-$code  \n";
}

$count    = 0;
$totalLen = 0;
$crc      = 0;
$hits     = 0;

foreach (explode("\n", $blob) as $line) {
    if ($line === '') {
        continue;
    }
    $fields = explode(',', $line);
    foreach ($fields as $k => $f) {
        $fields[$k] = trim($f);
    }
    // Field 1 = level: uppercase it.
    $fields[1] = strtoupper($fields[1]);
    // Field 3 = path: rewrite the version prefix.
    $fields[3] = str_replace('/api/v1/', '/api/v2/', $fields[3]);
    // Substr / strpos sanity checks that gate a counter.
    if (substr($fields[3], 0, 8) === '/api/v2/' && strpos($fields[5], 'ok-') === 0) {
        $hits++;
    }
    $rebuilt   = implode('|', $fields);
    $totalLen += strlen($rebuilt);
    $crc       = crc32($rebuilt . $crc);
    $count++;
}

printf("lines=%d hits=%d totalLen=%d crc=%d\n", $count, $hits, $totalLen, $crc);
