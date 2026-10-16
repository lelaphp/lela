<?php
// #2832 ordered reads/immutable updates and relative-base workload proposal.
$sum = 0;
for ($i = 0; $i < 1000; $i++) {
    $u = new Uri\WhatWg\Url('https://user:pass@EXAMPLE.com:443/a/b?x#y');
    $sum += strlen($u->getAsciiHost()) + strlen($u->getPath());
    $v = $u->withPath('/x/y')->withScheme('http')->withPort(80)->resolve('../c');
    $sum += strlen($v->toAsciiString()) + strlen($u->toAsciiString());
}
echo $sum, "\n";
