<?php
date_default_timezone_set('UTC');
$date = new DateTime('2000-01-01');
set_error_handler(function ($level, $message) use ($date) {
    echo 'handler:', $date->format('Y-m-d'), "\n";
    $date->modify('+2 days');
    date_create_from_format('Y-m-d', 'bad');
    throw new Exception('diagnostic');
});
try { date_modify($date, 'not-a-date'); }
catch (Exception $e) { echo $e->getMessage(), "\n"; }
restore_error_handler();
echo 'after:', $date->format('Y-m-d'), "\n";
$errors = DateTime::getLastErrors();
echo 'last-errors:', $errors['error_count'], "\n";
try { $date->__construct('not-a-date'); }
catch (Exception $e) { echo 'constructor:', $e->getMessage(), "\n"; }
try { echo $date->format('Y-m-d'); }
catch (Error $e) { echo 'cleared:', $e->getMessage(), "\n"; }
