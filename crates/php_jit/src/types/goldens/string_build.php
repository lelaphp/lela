<?php
// A blob built line by line, as string_munge's first loop builds it: a path
// joined by concatenation, a line interpolated, its pieces strings and a
// long, and the line appended with .=, Core alone.
function k($a, $n) {
    $blob = "";
    for ($i = 0; $i < $n; $i++) {
        $path = "/api/" . $a . "-x";
        $blob .= "  $i , $a ,$path , ok  \n";
    }
    return strlen($blob);
}
echo k("z", 100), "\n";
