<?php
// mandelbrot — render the Mandelbrot set on an N×N grid.
// (Computer Language Benchmarks Game classic.)
//
// Exercises: tight nested float loops with a data-dependent early-exit inner
// loop (the escape test), plus integer bit packing. Branch-prediction and the
// float fast path dominate. Unlike n-body there's no array churn — it's almost
// pure arithmetic in registers/locals.
//
// To keep output tiny and trivially comparable across engines we DON'T emit the
// PBM bitmap; instead we fold every output byte into a checksum. Same N => same
// checksum on every correct engine.

$N = 1100; // @bench-param  (image is N×N pixels)

$iter = 50;
$limit = 2.0;
$limit2 = $limit * $limit;

$checksum = 0;
$bytesOut = 0;

for ($y = 0; $y < $N; $y++) {
    $ci = (2.0 * $y / $N) - 1.0;
    $bitNum = 0;
    $byteAcc = 0;
    for ($x = 0; $x < $N; $x++) {
        $cr = (2.0 * $x / $N) - 1.5;
        $zr = 0.0;
        $zi = 0.0;
        $zrzr = 0.0;
        $zizi = 0.0;
        $escaped = false;
        for ($i = 0; $i < $iter; $i++) {
            $zi = 2.0 * $zr * $zi + $ci;
            $zr = $zrzr - $zizi + $cr;
            $zrzr = $zr * $zr;
            $zizi = $zi * $zi;
            if ($zrzr + $zizi > $limit2) {
                $escaped = true;
                break;
            }
        }

        $byteAcc <<= 1;
        if (!$escaped) {
            $byteAcc |= 1; // in-set pixel
        }
        $bitNum++;

        if ($bitNum === 8) {
            $checksum = ($checksum + $byteAcc) & 0xFFFFFFFF;
            $bytesOut++;
            $byteAcc = 0;
            $bitNum = 0;
        }
    }
    // flush a partial byte at row end (matches PBM packing)
    if ($bitNum !== 0) {
        $byteAcc <<= (8 - $bitNum);
        $checksum = ($checksum + $byteAcc) & 0xFFFFFFFF;
        $bytesOut++;
    }
}

// checksum is masked to 32 bits, so %d is identical and portable across engines
printf("N=%d bytes=%d checksum=%d\n", $N, $bytesOut, $checksum);
