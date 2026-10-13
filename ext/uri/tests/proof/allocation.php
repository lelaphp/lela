<?php
use Uri\Rfc3986\Uri as R;
use Uri\WhatWg\Url as W;
// Warm class/runtime paths before each measured event; no wall-clock timing.
$warm = new R('https://example.com/a?b#c'); $warm->getHost();
$m = memory_get_usage(); $u = new R('https://example.com/a?b#c'); echo 'raw ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $u->getRawHost(); echo 'raw-read ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $u->getHost(); echo 'normalize ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $u->getHost(); echo 'again ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $copy = clone $u; echo 'clone ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $copy->getHost(); echo 'clone-normalize ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $v = $u->withPath('/x/y'); echo 'update ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); unset($u, $copy, $v); echo 'free ', memory_get_usage() - $m, "\n";
$warm = new W('https://example.com/a?b#c');
$m = memory_get_usage(); $u = new W('https://example.com/a?b#c'); echo 'whatwg ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); unset($u); echo 'whatwg-free ', memory_get_usage() - $m, "\n";
// Larger-than-initial-pool actual strings survive object destruction as reusable pool capacity.
$path = ''; for ($i = 0; $i < 12000; $i++) $path .= 'x';
$m = memory_get_usage(); $u = new W('https://example.com/' . $path); echo 'pool-grow ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); unset($u); echo 'pool-retain ', memory_get_usage() - $m, "\n";
$m = memory_get_usage(); $u = new W('https://example.com/' . $path); echo 'pool-reuse ', memory_get_usage() - $m, "\n";
unset($u);
