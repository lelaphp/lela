<?php
use Uri\WhatWg\Url as W;
class OnErrorsRelease {
    public function __destruct() {
        echo "destructor\n";
        var_dump((new W('https://nested.example/'))->getAsciiHost());
    }
}
$errors = new OnErrorsRelease;
$u = W::parse('https://h/a', null, $errors);
var_dump($u->toAsciiString(), $errors);
$errors = new OnErrorsRelease;
$u = W::parse('http://h:65536/', null, $errors);
var_dump($u, $errors[0]->type->name);
// Constructor failure leaves the caller's reference untouched.
$errors = 'sentinel';
try { new W('http://h:65536/', null, $errors); } catch (Throwable $e) {
    var_dump($errors, $e->errors[0]->type->name);
}
class TypedErrors { public int $errors = 7; }
$typed = new TypedErrors;
try { W::parse('https://h/', null, $typed->errors); } catch (Throwable $e) {
    var_dump($e->getMessage(), $typed->errors);
}
var_dump((new W('https://after.example/'))->getAsciiHost());
