<?php
// Pinned mapping/NFC/deviation/unsupported-check inputs, not modern IDNA.
$sum = 0;
for ($i = 0; $i < 1000; $i++) foreach (["https://bücher.example/", "https://e\u{0301}.example/", "https://faß.de/", "https://a\u{200D}b.example/", "https://אa.example/"] as $text) {
    $u = new Uri\WhatWg\Url($text);
    $sum += strlen($u->getAsciiHost()) + strlen($u->getUnicodeHost());
}
echo $sum, "\n";
