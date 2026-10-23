<?php
// dax-jit-bench's scripts/inline/array_write.php at $n = 3000: the types
// golden's program, a loop for array_fill, which an engine with no extension
// does not have, run before the call, since the recording starts at the call.
// A packed array of 1024 ints, written by a masked int key in a loop.
function filled($n) {
    $a = [];
    for ($i = 0; $i < $n; $i++) {
        $a[] = 0;
    }
    return $a;
}
function k($a, $n) {
    for ($i = 1; $i < $n; $i++) {
        $a[$i & 1023] = $i;
    }
    return $a[1023];
}
echo k(filled(1024), 3000), "\n";
