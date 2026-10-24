<?php
// The hash slice's goldens (#3339): each function's loop reads, tests,
// writes or fetches for writing an element of an array that is a hash, by
// a constant string, a variable's string and a long, or compounds an
// element with one of the operators an ASSIGN_DIM_OP compiles in place
// past `+`, `-` and `*`. Each runs once with enough turns for its loop to
// be recorded.
function read($h, $n) {
    $s = 0; $k = 'b';
    for ($i = 0; $i < $n; $i++) {
        $s += $h['a'] + $h[$k] + $h[7];
    }
    return $s;
}
function tested($h, $n) {
    $s = 0; $k = 'b';
    for ($i = 0; $i < $n; $i++) {
        if (isset($h['a'])) { $s++; }
        $s += isset($h[$k]) + isset($h[$i]);
    }
    return $s;
}
function written($n) {
    $h = ['a' => 0, 'b' => 0, 7 => 0]; $k = 'b';
    for ($i = 0; $i < $n; $i++) {
        $h['a'] = $i; $h[$k] = $i; $h[$i & 7] = $i;
    }
    return $h['a'] + $h[$k] + $h[7];
}
function fetched($n) {
    $h = ['a' => [], 'c' => 0]; $k = 'c';
    for ($i = 0; $i < $n; $i++) {
        $h['a'][$i & 3] = $i; $h[$k]++;
    }
    return $h['c'];
}
function divided($n) {
    $h = ['x' => 1.5, 'y' => 0];
    for ($i = 0; $i < $n; $i++) { $h['x'] /= 1.0001; }
    return $h['x'];
}
function modulo($n) {
    $h = ['x' => 7, 'y' => 0];
    for ($i = 0; $i < $n; $i++) { $h['x'] %= 1000; $h['x'] += $i; }
    return $h['x'];
}
function shifted_left($n) {
    $h = ['x' => 1, 'y' => 0];
    for ($i = 0; $i < $n; $i++) { $h['x'] <<= 1; $h['x'] &= 0xffff; }
    return $h['x'];
}
function shifted_right($n) {
    $h = ['x' => 1 << 40, 'y' => 0];
    for ($i = 0; $i < $n; $i++) { $h['x'] >>= 1; $h['x'] |= 1 << 40; }
    return $h['x'];
}
function anded($n) {
    $h = ['x' => -1, 'y' => 0];
    for ($i = 0; $i < $n; $i++) { $h['x'] &= ~($i & 1); }
    return $h['x'];
}
function ored($n) {
    $h = ['x' => 0, 'y' => 0];
    for ($i = 0; $i < $n; $i++) { $h['x'] |= $i & 255; }
    return $h['x'];
}
function xored($n) {
    $h = ['x' => 0, 'y' => 0];
    for ($i = 0; $i < $n; $i++) { $h['x'] ^= $i; }
    return $h['x'];
}
function joined($n) {
    $h = ['x' => '', 'y' => 0]; $s = 'ab';
    for ($i = 0; $i < $n; $i++) { $h['x'] .= $s; }
    return strlen($h['x']);
}
$h = ['a' => 1, 'b' => 2, 7 => 3, 9 => 4];
echo read($h, 300), " ", tested($h, 300), " ", written(300), " ", fetched(300), "\n";
echo divided(300), " ", modulo(300), " ", shifted_left(300), " ", shifted_right(300), "\n";
echo anded(300), " ", ored(300), " ", xored(300), " ", joined(300), "\n";
