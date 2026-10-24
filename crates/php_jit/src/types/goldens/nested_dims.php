<?php
// #3370's goldens: each function's loop reads, tests or coalesces an
// element of the temporary a fetch made, writes, compounds or fetches for
// writing through the `VAR` a fetch for writing left indirect, or writes
// the array a reference holds. Each runs once with enough turns for its
// loop to be recorded.
function nested_read($h, $n) {
    $s = 0;
    for ($i = 0; $i < $n; $i++) {
        if (isset($h['n'][$i & 1])) { $s++; }
        $s += $h['a']['b'] ?? 0;
        $s += $h['a']['c'];
    }
    return $s;
}
function nested_write($n) {
    $g = ['a' => [0, 0, 0, 0], 'b' => ['x' => 0], 'c' => [[0, 0], [0, 0]]];
    for ($i = 0; $i < $n; $i++) {
        $g['a'][$i & 3] = $i; $g['b']['x'] += $i; $g['c'][$i & 1][$i & 1] = 1;
    }
    return $g['a'][3] + $g['b']['x'] + $g['c'][1][1];
}
function ref_write(array &$r, $n) {
    for ($i = 0; $i < $n; $i++) {
        $r['k'] = $i; $r[$i & 3] = $i;
    }
}
$h = ['n' => [5, 6], 'a' => ['b' => 2, 'c' => 3]];
$r = ['k' => 0, 0, 0, 0, 0];
ref_write($r, 300);
echo nested_read($h, 300), " ", nested_write(300), " ", $r['k'] + $r[3], "\n";
