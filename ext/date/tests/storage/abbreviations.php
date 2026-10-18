<?php
ini_set('zend_test.zend_mm_custom_handlers.enabled', '1');
$zone = new DateTimeZone('EST');
$copy = clone $zone;
$date = new DateTime('2024-03-30 12:00 CEST');
$of_date = $date->getTimezone();
$in_zone = new DateTime('2024-03-30 12:00', $zone);
unset($in_zone, $of_date, $date, $copy, $zone);
ini_set('zend_test.zend_mm_custom_handlers.enabled', '0');
echo "done\n";
