<?php
// docs/perf/workloads/binary_trees.php at $N = 4, its printf an echo: the types goldens' program.
// binary_trees — allocate and walk many short-lived binary trees.
// (Computer Language Benchmarks Game classic; the canonical allocator/GC test.)
//
// Exercises: heavy allocation churn and reclamation. Nodes are nested arrays
// ([left, right], leaves are null), so every node is a heap allocation that
// becomes garbage almost immediately. This is the benchmark to watch under
// `/usr/bin/time` — peak RSS here tells you how the engine's allocator and GC
// behave, not just raw CPU speed.
//
// Deterministic output: per-depth check sums plus the long-lived tree's check.

$N = 4; // @bench-param  (maximum tree depth)

function make_tree(int $depth)
{
    if ($depth === 0) {
        return [null, null];
    }
    $d = $depth - 1;
    return [make_tree($d), make_tree($d)];
}

function check_tree($node): int
{
    if ($node[0] === null) {
        return 1;
    }
    return 1 + check_tree($node[0]) + check_tree($node[1]);
}

$minDepth = 4;
$maxDepth = $N < $minDepth + 2 ? $minDepth + 2 : $N;
$stretchDepth = $maxDepth + 1;

$stretch = make_tree($stretchDepth);
echo $stretchDepth, check_tree($stretch), "\n";

$longLived = make_tree($maxDepth);

for ($depth = $minDepth; $depth <= $maxDepth; $depth += 2) {
    $iterations = 1 << ($maxDepth - $depth + $minDepth);
    $check = 0;
    for ($i = 0; $i < $iterations; $i++) {
        $check += check_tree(make_tree($depth));
    }
    echo $iterations, $depth, $check, "\n";
}

echo $maxDepth, check_tree($longLived), "\n";
