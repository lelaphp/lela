<?php
class Shape {
    public $n = 0;
    function grow($x) {
        for ($i = 0; $i < 10; $i++) {
            if ($x instanceof Shape) {
                $this->n += $i;
            }
            if ($x instanceof self) {
                $this->n++;
            }
        }
        return $this->n;
    }
}
$s = new Shape;
for ($j = 0; $j < 5; $j++) {
    $s->grow($s);
}
echo $s->n, "\n";
