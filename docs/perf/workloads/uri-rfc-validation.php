<?php
// #2832 correctness-qualified workload proposal; no tolerance selected.
$inputs = ['http://h/a?x#y', '../a', 'x://', 'http://[::1]/', 'http://h/%zz', "http://h/a\0b"];
$accepted = 0;
for ($i = 0; $i < 1000; $i++) foreach ($inputs as $text) {
    if (Uri\Rfc3986\Uri::parse($text) !== null) $accepted++;
}
echo $accepted, "\n";
