<?php
date_default_timezone_set('UTC');
$date = new DateTime('2024-03-30 12:00 Europe/Amsterdam');
$clone = clone $date;
$date->modify('+1 day');
echo $date->format('Y-m-d H:i:s P e'), "\n", $clone->format('Y-m-d H:i:s P e'), "\n";
$interval = new DateInterval('P1DT2H');
echo $date->add($interval)->format('c'), "\n";
$immutable = new DateTimeImmutable('2024-10-27 01:30 Europe/Amsterdam');
echo $immutable->modify('+2 hours')->format('c T'), "\n", $immutable->format('c T'), "\n";
foreach ([-9223372036854775807, -1, 0, 9223372036854775807] as $timestamp) {
    $epoch = new DateTime('@' . $timestamp);
    echo $epoch->format('U Y-m-d'), "\n";
}
$period = new DatePeriod(new DateTime('2024-01-01'), new DateInterval('P1D'), 2);
foreach ($period as $point) { echo $point->format('Y-m-d'), "\n"; }
$before = memory_get_usage();
$allocated = new DateInterval('P2DT3H');
$after = memory_get_usage();
$copy = clone $allocated;
echo 'interval:', $after - $before, ':clone:', memory_get_usage() - $after, "\n";
$before = memory_get_usage();
unset($allocated, $copy);
echo 'interval-release:', memory_get_usage() - $before, "\n";
var_dump(date_parse_from_format('!Y-m-d', '2024-02-31')['warning_count']);
var_dump(DateTime::createFromFormat('Y-m-d', 'bad'));
$errors = DateTime::getLastErrors();
echo 'errors:', $errors['warning_count'], ':', $errors['error_count'], "\n";
new DateTime('2024-01-01');
var_dump(DateTime::getLastErrors());
