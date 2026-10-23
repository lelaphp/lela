<?php
// docs/perf/workloads/fannkuch.php at $N = 7: the program of the record
// `types`' tests build fannkuch's side traces from, and echo in place of
// printf, which an engine with no extension does not have.
// fannkuch-redux — count pancake flips over every permutation of 1..n.
// (Computer Language Benchmarks Game classic.)
//
// Exercises: integer array manipulation (in-place reversals + index juggling),
// branch-heavy control flow, and O(n!) iteration. No floats, no allocation in
// the hot loop — it's the integer / array-mutation counterpart to fib's pure
// call test.
//
// Deterministic output: a checksum and the maximum flip count for n.

$N = 7;

function fannkuch(int $n): array
{
    $perm = [];
    $perm1 = [];
    $count = [];
    for ($i = 0; $i < $n; $i++) {
        $perm1[$i] = $i;
    }

    $maxFlips = 0;
    $checksum = 0;
    $permCount = 0;
    $r = $n;

    while (true) {
        while ($r !== 1) {
            $count[$r - 1] = $r;
            $r--;
        }

        for ($i = 0; $i < $n; $i++) {
            $perm[$i] = $perm1[$i];
        }

        $flips = 0;
        $k = $perm[0];
        while ($k !== 0) {
            // reverse perm[0..k]
            $lo = 0;
            $hi = $k;
            while ($lo < $hi) {
                $t = $perm[$lo];
                $perm[$lo] = $perm[$hi];
                $perm[$hi] = $t;
                $lo++;
                $hi--;
            }
            $flips++;
            $k = $perm[0];
        }

        if ($flips > $maxFlips) {
            $maxFlips = $flips;
        }
        // alternating sign checksum
        $checksum += ($permCount % 2 === 0) ? $flips : -$flips;

        // next permutation (incremental, Mihai/CLBG style)
        while (true) {
            if ($r === $n) {
                return [$checksum, $maxFlips];
            }
            $perm0 = $perm1[0];
            $i = 0;
            while ($i < $r) {
                $j = $i + 1;
                $perm1[$i] = $perm1[$j];
                $i = $j;
            }
            $perm1[$r] = $perm0;

            $count[$r]--;
            if ($count[$r] > 0) {
                break;
            }
            $r++;
        }

        $permCount++;
    }
}

[$checksum, $maxFlips] = fannkuch($N);
echo $checksum, "\nPfannkuchen(", $N, ") = ", $maxFlips, "\n";
