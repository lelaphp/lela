<?php
// RFC byte/percent normalization, explicitly not WHATWG/IDNA.
$sum = 0;
for ($i = 0; $i < 1000; $i++) foreach (['http://h/%c3%a9', 'http://h/%65%cc%81', 'http://b%C3%BCcher.example/%7e'] as $text) {
    $u = new Uri\Rfc3986\Uri($text);
    $sum += strlen($u->toString()) + strlen($u->toRawString());
}
echo $sum, "\n";
