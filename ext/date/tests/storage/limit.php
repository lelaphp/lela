<?php
date_default_timezone_set('UTC');
$dates = [];
for ($i = 0; $i < 100000; $i++) {
    $dates[] = new DateTime('2024-01-01 Europe/Amsterdam');
}
echo "unexpected success\n";
