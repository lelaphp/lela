<?php
// #2832 raw -> normalized -> clone/update -> resolve workload proposal.
$sum = 0;
for ($i = 0; $i < 1000; $i++) {
    $u = new Uri\Rfc3986\Uri('HTTP://EXAMPLE.COM/a/./b/../%7e?q=%41');
    $sum += strlen($u->getRawPath()) + strlen($u->getPath());
    $v = $u->withPath('/x/y')->resolve('../c');
    $sum += strlen($v->toString()) + strlen($u->toRawString());
}
echo $sum, "\n";
