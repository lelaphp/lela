<?php
// A line held against the empty string and another, its length summed and
// a count cast to a boolean, as string_munge's line loop compares and
// measures its lines: strict comparisons of strings, STRLEN and BOOL, Core
// alone.
function k($lines, $n) {
    $c = 0;
    for ($i = 0; $i < $n; $i++) {
        $s = $lines[$i % 4];
        if ($s === '') {
            continue;
        }
        $same = $s !== "zz";
        $c += strlen($s) + (int) $same + (int) (bool) $i;
    }
    return $c;
}
echo k(["ab", "", "zz", "m"], 100), "\n";
