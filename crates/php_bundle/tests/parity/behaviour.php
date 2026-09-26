<?php
var_dump(strspn("42 is the answer", "1234567890"), strspn("foobar", "o", 1), strspn("foobar", "o", 1, 1));
var_dump(strspn("a", "b", null));
try { strspn(); } catch (ArgumentCountError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { strspn("a", "b", 1, null, 5); } catch (ArgumentCountError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { strspn([], "b"); } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { strspn("a", "b", length: []); } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
var_dump(explode(",", "a,b,c"), explode(separator: ",", string: "x,y"), explode(",", "a,b,c", 2));
try { explode("", "a"); } catch (ValueError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
var_dump(round(2.5), round(-2.5), round(2.5, 0, RoundingMode::HalfEven), round(3.5, mode: RoundingMode::HalfEven), round(7), round("4.5"));
try { round("x"); } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { round(1.5, 0, "nope"); } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
var_dump(trim("  hi \n"), trim("xxhixx", "x"), trim("\v\0a\t"), trim(12));
var_dump(str_replace("a", "o", "banana", $count), $count, str_replace("n", "", "banana"));
var_dump(in_array("1", [1, 2]), in_array("1", [1, 2], true), in_array(3, [1, 2]));
var_dump(array_keys(["a" => 1, "b" => 2, "c" => 1]), array_keys(["a" => 1, "b" => 2, "c" => 1], 1), array_keys([1, "1", 1.0], "1", true));
try { array_keys(["a" => 1], strict: true); } catch (ArgumentCountError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
$stack = [1, 2]; var_dump(array_push($stack, 3, 4), $stack);
$shared = [9]; $copy = $shared; array_push($shared, 10); var_dump($shared, $copy);
try { $s = "str"; array_push($s, 1); } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { array_push($undefined, 1); } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
var_dump(function_exists('sapi_windows_vt100_support'), function_exists('join'), (new ReflectionProperty('DOMNode', 'nodeName'))->isVirtual());
var_dump(ProtoLevel::from(2), ProtoLevel::Low->value, ProtoLevel::TOP, ProtoLevel::tryFrom(3), ProtoLevel::High instanceof BackedEnum);
try { $a = []; array_push($a, extra: 1); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
var_dump(RoundingMode::HalfEven instanceof UnitEnum, Dom\AdjacentPosition::from("afterend"), Dom\AdjacentPosition::AfterBegin->value, Dom\AdjacentPosition::tryFrom("x"));
var_dump(utf8_encode("x"), FILE_TEXT, PROTO_BUILT, Proto\INSIDE);
try { proto_raw(null); } catch (ValueError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { proto_raw([]); } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
var_dump(proto_raw("5"), proto_seen(), proto_seen(false), proto_seen(flag: true));
var_dump(proto_call(fn ($a, $b) => "$a-$b", b: 2, a: 1), proto_call('strspn', "aab", "a"));
class Bag implements IteratorAggregate, Countable { function getIterator(): Generator { yield 1; } function count(): int { return 3; } }
var_dump(proto_count([1, 2]), proto_count(new Bag));
try { proto_count("x"); } catch (TypeError $e) { echo explode(", called in", $e->getMessage())[0], "\n"; }
var_dump(proto_manual(), proto_manual(1, 2));
try { proto_manual(1); } catch (ArgumentCountError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
class Shown { function __toString(): string { return "shown"; } }
var_dump(proto_describe(5), proto_describe(new Shown), proto_false());
try { proto_never(); } catch (Exception $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
$b = new ProtoBox();
var_dump($b->value(), $b instanceof ProtoCounter, $b instanceof ProtoBase, $b->label, ProtoBox::MIXED, ProtoBox::LIST);
$b->grow(); var_dump($b->value(), $b->add(1)->add(2)->value(), $b->kind(), $b->done());
var_dump(ProtoBox::make(7)->value(), ProtoBox::START, (new ProtoBox(3))->value(), (new ProtoTally())->value(), (new ProtoLaid())->same());
try { new ProtoBase(); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { $b->hidden(); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
