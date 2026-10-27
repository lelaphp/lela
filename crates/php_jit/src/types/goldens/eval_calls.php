<?php
// #3561's and #3570's goldens. `Evald` is a class an eval declares, so its
// methods' op arrays are the request's own, which no trace may name. Its
// method's call on a variable's object and its static method's call
// through its name are pushed in place under the guard on the pair the
// opline's runtime-cache slot holds, the callee held by its record and
// its frame laid with the room the recording's request laid; the trace
// stops at the call as php's STOP_INTERPRETER, and the interpreter makes it.
eval('class Evald {
    public $b = 1;
    public function get($v) { return $this->b + $v; }
    public static function twice($v) { return $v + $v; }
}');
function evals($o, $n) {
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = $o->get($i);
    }
    return $s;
}
function evalstatics($n) {
    $s = 0;
    for ($i = 1; $i < $n; $i++) {
        $s = Evald::twice($i);
    }
    return $s;
}
echo evals(new Evald, 100), " ", evalstatics(100), "\n";
