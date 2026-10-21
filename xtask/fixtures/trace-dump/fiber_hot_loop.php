<?php
$fiber = new Fiber(function (): int {
    $sum = 0;
    for ($i = 0; $i < 50; $i++) {
        $sum += $i;
        Fiber::suspend($sum);
    }
    return $sum;
});
$seen = $fiber->start();
while (!$fiber->isTerminated()) {
    $last = $seen;
    $seen = $fiber->resume();
}
echo $last, " ", $fiber->getReturn(), "\n";
