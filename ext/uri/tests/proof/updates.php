<?php
use Uri\Rfc3986\Uri as R;
use Uri\WhatWg\Url as W;
$r = new R('HTTP://EXAMPLE.com/a/./b');
var_dump($r->toString());
foreach (['withHost' => 'bad host', 'withScheme' => '1bad', 'withPath' => '/%zz'] as $method => $text) {
    try { $r->$method($text); } catch (Throwable $e) { var_dump($e->getMessage()); }
    var_dump($r->toRawString(), $r->toString());
}
var_dump($r->withQuery('')->getRawQuery(), $r->withQuery(null)->getRawQuery());
$w = new W('https://user:pass@h:444/a');
foreach (['withScheme' => 'file', 'withHost' => 'bad host', 'withPort' => 65536] as $method => $text) {
    try { var_dump($w->$method($text)->toAsciiString()); } catch (Throwable $e) {
        var_dump($e->getMessage());
        foreach ($e->errors as $error) var_dump($error->type->name, $error->failure, $error->context);
    }
    var_dump($w->toAsciiString());
}
$opaque = new W('mailto:user@example.com');
var_dump($opaque->withPath('replacement')->toAsciiString());
try { $opaque->withUsername('x'); } catch (Throwable $e) { var_dump($e->getMessage(), $e->errors); }
var_dump((new W('https://h/a'))->withScheme('http')->withPort(80)->toAsciiString());
var_dump($w->withQuery('')->getQuery(), $w->withQuery(null)->getQuery());
