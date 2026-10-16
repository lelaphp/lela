<?php
ini_set('zend_test.zend_mm_custom_handlers.enabled', '1');
$zone = new DateTimeZone('Europe/Amsterdam');
$date = new DateTime('2024-03-30 12:00 Europe/Amsterdam');
$copy = clone $date;
$date->modify('+1 day');
$interval = new DateInterval('P1DT2H');
$interval_copy = clone $interval;
unset($interval_copy, $interval, $copy, $date, $zone);
ini_set('zend_test.zend_mm_custom_handlers.enabled', '0');
echo "done\n";
