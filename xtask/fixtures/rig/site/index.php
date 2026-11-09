<?php
// The fixture rig's one page: it answers 301 to any address but its
// site's, as a site does, reads its database, appends a byte to it and
// logs, as a counting build's line, how many bytes it read.
if ($_SERVER['HTTP_HOST'] !== '127.0.0.1:8099') {
    http_response_code(301);
}
$live = __DIR__ . '/live.txt';
$read = file_get_contents($live);
file_put_contents($live, $read . 'x');
file_put_contents('php://stderr', 'dispatch-census Read' . strlen($read) . " 1\n");
echo $read;
