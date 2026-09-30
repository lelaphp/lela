--TEST--
The runner puts a php-fpm where FPM\Tester::findExecutable looks last
--SKIPIF--
<?php if (!is_executable(dirname(__DIR__) . '/php-fpm')) die('skip php-fpm binary not found'); ?>
--FILE--
<?php
$version = shell_exec(escapeshellarg(dirname(__DIR__) . '/php-fpm') . ' -n -v');
var_dump(str_contains($version, '(fpm-fcgi)'));
--EXPECT--
bool(true)
