<?php
ini_set('memory_limit', '16M');
$path = ''; for ($i = 0; $i < 250000; $i++) $path .= 'x';
$held = [];
for ($i = 0; $i < 40; $i++) {
    $held[] = new Uri\Rfc3986\Uri('https://example.com/' . $path);
}
foreach ($held as $u) {
    echo "normalized-read\n";
    $u->getHost();
}
