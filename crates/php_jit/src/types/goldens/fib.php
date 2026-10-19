<?php
// docs/perf/workloads/fib.php at $N = 6: the types golden's program.
// fib — naive recursive Fibonacci.
//
// Exercises: function-call overhead, the interpreter dispatch loop, integer
// arithmetic, and deep-ish recursion. This is the purest test of "how much
// does it cost to call a function and come back" — almost no builtins, no
// allocation, no floats. If Mahout is slow here, it's call/dispatch overhead.
//
// Deterministic output: fib(N). Same on every engine.

$N = 6; // @bench-param  (which Fibonacci number to compute)

function fib(int $n): int
{
    if ($n < 2) {
        return $n;
    }
    return fib($n - 1) + fib($n - 2);
}

echo fib($N), "\n";
