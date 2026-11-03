//! A native's variadic tail as `Z_PARAM_VARIADIC` reads it (#4014): in
//! place over the frame's slots, so a call of any length allocates
//! nothing past the request's own heap for a tail of a type that takes
//! each argument as it stands, and `Z_PARAM_VARIADIC_WITH_NAMED` lays
//! its list of named arguments down only where the call names one. The
//! test binary's allocator counts what the system allocator serves on
//! this thread, which a program reads through `allocations()`.

use php_heap::CountingSystem;
use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder, NamedVariadic, Variadic};
use crate::api::values::{PhpArray, Value};
use crate::engine::Engine;
use crate::test::Capture;

#[global_allocator]
static SYSTEM: CountingSystem = CountingSystem;

/// `allocations(): int`: what the system allocator has served the
/// watched thread so far.
#[php_function]
fn allocations() -> i64 {
    i64::try_from(CountingSystem::allocations()).unwrap_or(i64::MAX)
}

/// `tail_sum(mixed ...$values): int`: the ints of the tail summed,
/// each element read.
#[php_function]
fn tail_sum(values: Variadic<Value>) -> i64 {
    values.filter_map(|value| value.as_int()).sum()
}

/// `tail_sum3(mixed $first, mixed ...$values): int`, frameless for up
/// to three arguments: `tail_sum` over all of them, the last read first
/// and the middle one by position.
#[php_function(frameless(2, 3))]
fn tail_sum3(first: Value, values: Variadic<Value>) -> i64 {
    let last = values.clone().next_back().and_then(|value| value.as_int());
    let middle = values.clone().next().and_then(|value| value.as_int());
    let all: i64 = values.filter_map(|value| value.as_int()).sum();
    first.as_int().unwrap_or(0) * 1000 + last.unwrap_or(0) * 100 + middle.unwrap_or(0) * 10 + all
}

/// `tail_count(array ...$arrays): int`: the elements of the arrays
/// counted, each held to `array` on entry.
#[php_function]
fn tail_count(arrays: Variadic<PhpArray>) -> i64 {
    arrays
        .map(|array| i64::try_from(array.len()).unwrap_or(i64::MAX))
        .sum()
}

/// `named_tail(mixed ...$args): int`: how many positional arguments,
/// and ten times how many named ones.
#[php_function]
fn named_tail(args: NamedVariadic<Value>) -> i64 {
    let named = i64::try_from(args.named().len()).unwrap_or(i64::MAX);
    let positional = i64::try_from(args.positional.len()).unwrap_or(i64::MAX);
    positional + 10 * named
}

fn tails() -> Extension {
    ExtensionBuilder::default()
        .functions(&[
            allocations::DECLARED,
            tail_sum::DECLARED,
            tail_sum3::DECLARED,
            tail_count::DECLARED,
            named_tail::DECLARED,
        ])
        .build("variadic_tails")
}

/// `src` served by a CLI engine with the tails declared, what it wrote.
fn served(src: &str) -> String {
    let engine = Engine::cli_builder().extensions(&[tails()]).build();
    let served = engine.serve_capture(src, Capture::default());
    let stdout = served.stdout_text();
    assert_eq!(served.exit_code, 0, "{stdout}");
    stdout
}

/// A loop of calls with a tail of two, five and twenty arguments read
/// in place, of a frameless call's tail of two, and of a named tail with
/// no argument named: none of the calls allocates, and a named tail
/// with one named allocates its list.
#[test]
fn a_tail_read_in_place_allocates_nothing_per_call() {
    CountingSystem::watch();
    let stdout = served(
        "<?php\n\
         $x = 4; $r = [1, &$x];\n\
         echo tail_sum(1, 2), ' ', tail_sum(1, 2, 3, 4, 5), ' ', tail_sum(...[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]), ' ', tail_sum(...$r), \"\\n\";\n\
         $a = [1, 2]; $b = [3]; $p = 1; $q = 2; $t = 3;\n\
         echo tail_count($a, $b), ' ', tail_count($a, $b, $a, $b, []), ' ', named_tail(1, 2, 3), ' ', named_tail(1, x: 2), \"\\n\";\n\
         $s = allocations();\n\
         for ($i = 0; $i < 100; $i++) {\n\
             tail_sum(1, 2); tail_sum(1, 2, 3, 4, 5);\n\
             tail_sum(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20);\n\
             tail_count($a, $b); tail_count($a, $b, $a, $b, []);\n\
             tail_sum3($p, $q, $t); named_tail(1, 2, 3);\n\
         }\n\
         echo allocations() - $s, ' ';\n\
         $s = allocations();\n\
         for ($i = 0; $i < 100; $i++) { named_tail(1, x: 2); }\n\
         echo allocations() - $s > 0 ? 'some' : 'none', \"\\n\";\n",
    );
    assert_eq!(stdout, "3 15 210 5\n3 6 3 11\n0 some\n");
}

/// An `array` tail is held to its type on entry, at the argument that is
/// none, before the body runs.
#[test]
fn an_array_tail_refuses_on_entry_at_the_argument_that_is_none() {
    let stdout = served(
        "<?php\n\
         try { tail_count([1], [2], 3, [4]); } catch (TypeError $e) { echo $e->getMessage(), \"\\n\"; }\n",
    );
    assert_eq!(
        stdout,
        "tail_count(): Argument #3 must be of type array, int given\n"
    );
}

/// A frameless call's tail is read in place over its operands, from
/// either end and by position.
#[test]
fn a_frameless_tail_is_read_in_place_over_the_operands() {
    let stdout = served(
        "<?php\n\
         $p = 1; $q = 2; $r = 3;\n\
         echo tail_sum3($p, $q), ' ', tail_sum3($p, $q, $r), \"\\n\";\n",
    );
    assert_eq!(stdout, "1222 1325\n");
}
