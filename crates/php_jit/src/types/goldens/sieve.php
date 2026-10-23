<?php
// dax-jit-bench's scripts/inline/sieve.php at $n = 1000: the types golden's
// program, a loop for array_fill, which an engine with no extension does not
// have, run before the call, since the recording starts at the call.
// Sieve of Eratosthenes over a packed array of bools, then a count of the primes.
function filled($n) {
    $a = [];
    for ($i = 0; $i < $n; $i++) {
        $a[] = true;
    }
    return $a;
}
function k($a, $n) {
    $a[0] = false;
    $a[1] = false;
    for ($i = 2; $i * $i < $n; $i++) {
        if ($a[$i]) {
            for ($j = $i * $i; $j < $n; $j += $i) {
                $a[$j] = false;
            }
        }
    }
    $c = 0;
    for ($i = 0; $i < $n; $i++) {
        if ($a[$i]) {
            $c++;
        }
    }
    return $c;
}
echo k(filled(1000), 1000), "\n";
