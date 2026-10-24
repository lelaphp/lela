<?php
// A point a function holds, read and written from outside its class: a
// read, a write of a string, a compound write and a decrement, each on a
// published class's declared slot, under one class guard; a typed
// property's write of a long in place, and of a string its type
// coerces the handler's; a decrement whose result is read the handler's,
// Core alone.
class Point {
    public $x = 0;
    public $y = 0;
    public $tag = "";
    public int $n = 0;
}
function k($p, $m) {
    $t = 0;
    for ($i = 0; $i < $m; $i++) {
        $t += $p->x;
        $p->tag = "p";
        $p->y -= 2;
        $p->x--;
        $p->n = $i;
        $p->n = "7";
        $t += $p->y--;
    }
    return $t + $p->n;
}
echo k(new Point, 100), "\n";
