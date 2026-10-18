<?php
$start = new DateTime('2024-03-30 12:00 UTC');
$step = new DateInterval('P1D');
ini_set('zend_test.zend_mm_custom_handlers.enabled', '1');
$period = new DatePeriod($start, $step, 2);
$copy = clone $period;
$interval = $copy->getDateInterval();
$iso = DatePeriod::createFromISO8601String('R2/2012-07-01T00:00:00Z/P7D');
unset($copy, $interval, $iso, $period);
ini_set('zend_test.zend_mm_custom_handlers.enabled', '0');
echo "done\n";
