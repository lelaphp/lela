<?php
// docs/perf/workloads/spectralnorm.php at $N = 6: the types golden's program,
// a loop for array_fill, ** 0.5 for sqrt and echo in place of printf, which an
// engine with no extension does not have.
// spectralnorm — largest singular value of an N×N matrix via power iteration.
// (Computer Language Benchmarks Game classic.)
//
// Exercises: float arithmetic + array reads/writes in O(N^2) inner loops, with
// an integer-division-heavy matrix-element function called on the hot path.
// Complements n-body (which is array-of-records) with flat numeric arrays.
//
// Deterministic output: the spectral norm to 9 decimals.

$N = 6; // @bench-param  (matrix dimension)

// A[i][j] = 1 / ((i+j)(i+j+1)/2 + i + 1)
function eval_A(int $i, int $j): float
{
    $ij = $i + $j;
    return 1.0 / ($ij * ($ij + 1) / 2 + $i + 1);
}

function eval_A_times_u(int $n, array $u): array
{
    $out = [];
    for ($i = 0; $i < $n; $i++) {
        $sum = 0.0;
        for ($j = 0; $j < $n; $j++) {
            $sum += eval_A($i, $j) * $u[$j];
        }
        $out[$i] = $sum;
    }
    return $out;
}

function eval_At_times_u(int $n, array $u): array
{
    $out = [];
    for ($i = 0; $i < $n; $i++) {
        $sum = 0.0;
        for ($j = 0; $j < $n; $j++) {
            $sum += eval_A($j, $i) * $u[$j];
        }
        $out[$i] = $sum;
    }
    return $out;
}

function eval_AtA_times_u(int $n, array $u): array
{
    return eval_At_times_u($n, eval_A_times_u($n, $u));
}

$u = [];
$v = [];
for ($i = 0; $i < $N; $i++) {
    $u[] = 1.0;
    $v[] = 0.0;
}

for ($i = 0; $i < 10; $i++) {
    $v = eval_AtA_times_u($N, $u);
    $u = eval_AtA_times_u($N, $v);
}

$vBv = 0.0;
$vv = 0.0;
for ($i = 0; $i < $N; $i++) {
    $vBv += $u[$i] * $v[$i];
    $vv += $v[$i] * $v[$i];
}

echo ($vBv / $vv) ** 0.5, "\n";
