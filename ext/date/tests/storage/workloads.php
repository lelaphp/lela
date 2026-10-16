<?php
// Content-pinned candidate workloads, not an approved performance bar.
// The CI caller selects exactly one case and supplies paired/A-A measurement.
date_default_timezone_set('UTC');
$case = $argv[1] ?? 'parse';
$count = 20000;
if ($case === 'parse') {
    for ($i = 0; $i < $count; $i++) {
        $value = strtotime('2024-03-30 12:34:56 Europe/Amsterdam', 0);
    }
    echo $value, "\n";
} elseif ($case === 'construct') {
    for ($i = 0; $i < $count; $i++) {
        $date = new DateTime('2024-03-30 12:34:56 Europe/Amsterdam');
    }
    echo $date->format('c'), "\n";
} elseif ($case === 'modify') {
    $date = new DateTime('2024-03-30 12:34:56 Europe/Amsterdam');
    for ($i = 0; $i < $count; $i++) { $date->modify('+1 day'); }
    echo $date->format('c'), "\n";
} elseif ($case === 'format') {
    $date = new DateTime('2024-03-30 12:34:56 Europe/Amsterdam');
    for ($i = 0; $i < $count; $i++) { $value = $date->format('Y-m-d H:i:s.u P e T'); }
    echo $value, "\n";
} elseif ($case === 'warm-zone') {
    $first = new DateTimeZone('Europe/Amsterdam');
    for ($i = 0; $i < $count; $i++) { $zone = new DateTimeZone('Europe/Amsterdam'); }
    echo $zone->getName(), "\n";
} elseif ($case === 'invalid') {
    for ($i = 0; $i < $count; $i++) { $date = DateTime::createFromFormat('!Y-m-d', 'bad'); }
    echo DateTime::getLastErrors()['error_count'], "\n";
}
