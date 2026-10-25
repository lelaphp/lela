<?php
// #3346's golden: a class of the cached file linked over a class the
// request declared, `zend_lazy_class_load`'s copy, is the request's own,
// so its property oplines read their place from the request's runtime
// cache slot rather than a published class's.
eval('class Base { public $x = 1; public $y = 0; public int $n = 0; }');
class Child extends Base { }
function scoped($o, $m) {
    $t = 0;
    for ($i = 0; $i < $m; $i++) {
        $t += $o->x; $o->y = $i; $o->x += 1; $o->y++; $o->n = $i;
    }
    return $t;
}
echo scoped(new Child, 300), "\n";
