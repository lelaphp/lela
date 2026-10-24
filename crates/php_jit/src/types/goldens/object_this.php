<?php
// A counter's loop over its own properties, as Zend/micro_bench.php's
// methods loop over $this: a read, a write, a compound write, an
// increment and an element appended through a fetch for writing, each
// of a published class's declared slots in place, and an isset() the
// handler serves, Core alone.
class Counter {
    public $n = 0;
    public $s = 0.5;
    public $seen = [];
    function run($k) {
        $x = 0;
        for ($i = 0; $i < $k; $i++) {
            $x += $this->n;
            $this->n = $i;
            $this->s += 1.5;
            $this->n++;
            $this->seen[] = $i;
            $x += isset($this->s);
        }
        return $x;
    }
}
echo (new Counter)->run(100), "\n";
