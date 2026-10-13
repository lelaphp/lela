<?php
// #2832 special, opaque, file, IP and malformed validation workload proposal.
$inputs = ['https://h/a', 'mailto:a@b', 'file:///C:/a', 'https://0x7f.1', 'http://h:65536', 'https://EXA%23MPLE.org'];
$accepted = 0;
for ($i = 0; $i < 1000; $i++) foreach ($inputs as $text) {
    if (Uri\WhatWg\Url::parse($text) !== null) $accepted++;
}
echo $accepted, "\n";
