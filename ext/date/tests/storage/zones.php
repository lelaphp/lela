<?php
date_default_timezone_set('UTC');
$names = ['Europe/Amsterdam', 'America/New_York', 'Asia/Tokyo', 'Europe/London', 'Australia/Sydney', 'Africa/Cairo', 'America/Chicago', 'Europe/Paris', 'Asia/Kolkata', 'Pacific/Auckland'];
$zones = [];
foreach ($names as $name) {
    $before = memory_get_usage();
    $zones[] = new DateTimeZone($name);
    echo $name, ':', memory_get_usage() - $before, "\n";
}
$before = memory_get_usage();
$again = new DateTimeZone('Europe/Amsterdam');
echo 'repeat:', memory_get_usage() - $before, "\n";
$before = memory_get_usage();
unset($again);
echo 'release:', memory_get_usage() - $before, "\n";
$before = memory_get_usage();
$alias = new DateTimeZone('europe/amsterdam');
echo 'alias:', memory_get_usage() - $before, ':', $alias->getName(), "\n";
for ($j = 0; $j < 2; $j++) {
    try { new DateTimeZone('Mars/Olympus'); }
    catch (Exception $e) { echo $e->getMessage(), "\n"; }
}
$before = memory_get_usage();
$abbreviation = new DateTimeZone('EST');
$after = memory_get_usage();
$cloned = clone $abbreviation;
echo 'abbr:', $after - $before, ':clone:', memory_get_usage() - $after, "\n";
$before = memory_get_usage();
unset($abbreviation, $cloned);
echo 'abbr-release:', memory_get_usage() - $before, "\n";
echo 'peak-at-least-usage:', memory_get_peak_usage() >= memory_get_usage() ? 'yes' : 'no', "\n";
