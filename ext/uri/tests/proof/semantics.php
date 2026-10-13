<?php
// Bounded pinned-standard inputs, not a replacement parser (#2832).
use Uri\Rfc3986\Uri as R;
use Uri\WhatWg\Url as W;
$r = new R('HTTP://Us%65r:p%61ss@EXAMPLE.COM:00080/a/./b/../%7e?q=%41#%62');
var_dump($r->toRawString(), $r->getRawHost(), $r->getRawPath());
var_dump($r->getHost(), $r->getPath(), $r->getQuery(), $r->getUserInfo());
var_dump($r->toRawString(), $r->toString(), (clone $r)->toString());
$base = new R('http://a/b/c/d;p?q');
foreach (['g', '../g', '?y', '#s', '//g', '', '../../../g', '/./g', 'http:g'] as $text) {
    var_dump($base->resolve($text)->toRawString());
}
var_dump(R::parse('x', new R('relative/base')));
foreach (['x:', 'x://', 'http://@h:?#', 'http://h', 'http://h/?#', 'http://[v1.a]/'] as $text) {
    $u = new R($text);
    var_dump($u->getRawHost(), $u->getRawUsername(), $u->getRawPassword(), $u->getPort(), $u->getRawQuery(), $u->getRawFragment());
}
foreach (['https://EXAMPLE.com:443/a/../b', 'ftp://h:21', 'ws://h:80', 'wss://h:443', 'file://localhost/C|/a', 'x://Host/a', 'mailto:user@example.com', 'https://0x7f.1/', 'https://[2001:db8:0:0:1:0:0:1]/', 'http://h/?#'] as $text) {
    $u = new W($text);
    var_dump($u->toAsciiString(), $u->getAsciiHost(), $u->getUsername(), $u->getPassword(), $u->getPort(), $u->getQuery(), $u->getFragment());
}
var_dump((new W('https://h/a/b'))->resolve('../c?x#y')->toAsciiString());
// Mapping, NFC, deviation, unsupported Bidi/Joiner checks, invalid UTF-8.
foreach (["https://bücher.example/", "https://e\u{0301}.example/", "https://é.example/", "https://faß.de/", "https://a\u{200D}b.example/", "https://אa.example/", "https://\xFF.example/"] as $text) {
    $u = W::parse($text, null, $errors);
    var_dump($u?->toAsciiString(), $u?->getUnicodeHost());
    foreach ($errors as $e) var_dump($e->type->name, $e->failure, $e->context);
}
