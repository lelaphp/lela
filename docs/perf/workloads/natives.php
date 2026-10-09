<?php
// #2209: a native-call-heavy loop. strlen and array_key_exists as the
// ticket names them (php compiles both to their own opcodes, ZEND_STRLEN
// and ZEND_ARRAY_KEY_EXISTS, where the argument is not unpacked), then
// DO_ICALL natives whose return is a scalar (intdiv), a view of an array
// element (current), a view of a refcounted string handed back unchanged
// (trim, strtolower: php's zend_string_copy, the Owner::take path) and a
// string key (array_key_first).
$N = 1000000;
$a = ['k' => 1, 'x' => 'abc'];
$s = str_repeat('hello ', 3) . 'x';
$n = 0;
for ($i = 0; $i < $N; $i++) { $n += strlen($s); }
for ($i = 0; $i < $N; $i++) { if (array_key_exists('k', $a)) { $n++; } }
for ($i = 0; $i < $N; $i++) { $n += intdiv($i, 3); }
for ($i = 0; $i < $N; $i++) { $n += current($a); }
for ($i = 0; $i < $N; $i++) { $v = trim($s); }
for ($i = 0; $i < $N; $i++) { $v = strtolower($s); }
for ($i = 0; $i < $N; $i++) { $k = array_key_first($a); }
echo $n, ' ', $v, ' ', $k, "\n";
