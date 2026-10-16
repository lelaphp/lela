<?php
ini_set('memory_limit', '16M');
$path = ''; for ($i = 0; $i < 250000; $i++) $path .= 'x';
$held = [];
for ($i = 0; $i < 100; $i++) {
    $held[] = new Uri\WhatWg\Url('https://example.com/' . $path);
    echo "parsed-held\n";
}
