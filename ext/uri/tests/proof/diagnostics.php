<?php
use Uri\WhatWg\Url as W;
// Contexts can be transformed-domain allocations, not original input ranges.
foreach (['https://EXA%23MPLE.org/path', "http://u%zz@h\\a%xx#f", "https://exa\0mple.org/x", "x://h/a\0b?x", 'http://1.2.3.4.5/', 'http://[::1', 'http://h:65536/', 'file://C:/a'] as $text) {
    $u = W::parse($text, null, $errors);
    var_dump($u?->toAsciiString());
    foreach ($errors as $e) var_dump($e->type->name, $e->failure, $e->context);
}
// A later parse must not invalidate previously returned contexts.
$u = W::parse('https://EXA%23MPLE.org/path', null, $saved);
W::parse('https://elsewhere.example/');
var_dump($saved[0]->context);
var_dump(Uri\WhatWg\UrlValidationErrorType::cases());
