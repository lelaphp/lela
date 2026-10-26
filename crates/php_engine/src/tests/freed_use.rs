//! The freed-use guard as a program meets it (#1990): a deliberately
//! faulty native takes an owner of a block the heap has just freed,
//! through the type's own addref and not the request's, and the debug
//! build refuses it at the refcounted header and ends the request in
//! the `InternalError` naming the line that asked: at the loop's next
//! sensing point when the native ran inside the program, and where the
//! request ends when it ran outside the loop, in a request-end function
//! or in a start that then failed.
//!
//! And the debug quarantine that makes the guard deterministic (#1991):
//! a value used after its block is freed and one of its size made is
//! refused the first time it happens, and a write into a freed block while its
//! bin holds it back is found as the block leaves.

use crate::api::values::ClassId;
use php_heap::Zval;
use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{PhpString, Value};
use crate::engine::{Engine, RequestDescription};
use crate::test::Capture;

/// A value of `kind` made out of the request's heap, with its one
/// owner; `None` for a kind that is not one of the five refcounted
/// kinds.
fn made(req: &mut Request, kind: &[u8]) -> Option<Zval> {
    match kind {
        b"string" => req
            .heap_mut()
            .string(b"a string of its own block")
            .ok()
            .map(Zval::string),
        b"array" => req.heap_mut().array().ok().map(Zval::array),
        b"object" => req
            .new_object_of("stdClass")
            .ok()
            .map(|object| object.as_value().to_zval()),
        b"reference" => req
            .heap_mut()
            .reference(Zval::null())
            .ok()
            .map(Zval::reference),
        b"resource" => {
            let kind = req.memory_mut().resources().register_type(b"freed", None);
            req.heap_mut().resource(kind).ok().map(Zval::resource)
        }
        _ => None,
    }
}

/// A value of `kind` made and its one owner given up, so the block it
/// names is freed.
fn freed(req: &mut Request, kind: &[u8]) -> Option<Zval> {
    let made = made(req, kind)?;
    req.release(Value::of_zval(made));
    Some(made)
}

/// One more owner of a freed `z` through its type's own addref.
fn addref_by_kind(z: Zval) {
    if let Some(s) = z.as_str() {
        s.addref();
    } else if let Some(a) = z.as_arr() {
        a.addref();
    } else if let Some(o) = z.as_obj() {
        o.addref();
    } else if let Some(r) = z.as_resource() {
        r.addref();
    } else if let Some(r) = z.as_reference() {
        r.addref();
    }
}

/// `freed_addref(string $kind): void`, faulty on purpose: a value of
/// `$kind` freed, then one more owner of it taken.
#[php_function]
fn freed_addref(req: &mut Request, kind: PhpString) {
    if let Some(z) = freed(req, kind.as_bytes()) {
        addref_by_kind(z);
    }
}

/// A request-end function as faulty: a string freed and one more owner
/// of it taken, with nothing executing.
fn freed_addref_at_rshutdown(req: &mut Request) {
    if let Some(z) = freed(req, b"string") {
        addref_by_kind(z);
    }
}

/// `freed_then_made(string $kind): void`, faulty on purpose: a value
/// of `$kind` freed, one more of its kind and size made, and then one
/// more owner of the freed one taken, the slot-reuse pattern of #1984's
/// union. A free list that handed the freed block straight back would
/// have the stale owner count the new value one more.
#[php_function]
fn freed_then_made(req: &mut Request, kind: PhpString) {
    let Some(stale) = freed(req, kind.as_bytes()) else {
        return;
    };
    let made = made(req, kind.as_bytes());
    addref_by_kind(stale);
    if let Some(made) = made {
        req.release(Value::of_zval(made));
    }
}

/// `freed_protected(): void`, faulty on purpose: an array freed and then
/// a walk's recursion guard set on it, a write into the freed block that
/// moves no refcount.
#[php_function]
fn freed_protected(req: &mut Request) {
    if let Some(a) = freed(req, b"array").and_then(Zval::as_arr) {
        a.protect();
    }
}

/// The line of `freed_read`'s read of the freed string's bytes.
const READ_AT: u32 = line!() + 9;

/// `freed_read(): int`, faulty on purpose: a string freed, then its
/// bytes read, the stale read of #1985-#1989's regressions; what it
/// read is the length it answers.
#[php_function]
fn freed_read(req: &mut Request) -> i64 {
    freed(req, b"string")
        .and_then(Zval::as_str)
        .map_or(-1, |s| i64::try_from(s.bytes().len()).unwrap_or(-1))
}

fn faulty() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            freed_read::declared(),
            freed_addref::declared(),
            freed_then_made::declared(),
            freed_protected::declared(),
        ])
        .build("freed_use")
}

/// The rendered fatal's text up to the site, and the site's file.
const REFUSED: &str = "Fatal error: InternalError: an addref of the block at 0x";
const SITE: &str = ", which the heap has freed, at crates/php_engine/src/tests/freed_use.rs:";

/// Each of the five kinds: the addref through the type's own path is
/// refused, the request ends in the `InternalError` at the native's
/// call, and nothing after it runs.
#[test]
fn an_addref_of_a_freed_block_ends_the_request_at_the_next_sensing_point() {
    let engine = Engine::cli_builder().extensions(&[faulty()]).build();
    for kind in ["string", "array", "object", "reference", "resource"] {
        let served = engine.serve_capture(
            &format!("<?php echo 'a';\nfreed_addref('{kind}');\necho 'went on';\n"),
            Capture::default(),
        );
        let stdout = served.stdout_text();
        assert!(
            stdout.starts_with(&format!("a\n{REFUSED}")),
            "{kind}: {stdout}"
        );
        assert!(stdout.contains(SITE), "{kind}: {stdout}");
        // The loop asks after the native's handler, which has moved
        // the frame to the next opline; the fatal names the call's line.
        assert!(stdout.contains(" on line 2\n"), "{kind}: {stdout}");
        assert!(!stdout.contains("went on"), "{kind}: {stdout}");
        assert_eq!(served.exit_code, 255, "{kind}");
    }
}

/// A refusal made in a request-end function, where no handler runs
/// after it, is raised at the request's end, with nothing executing.
#[test]
fn an_addref_of_a_freed_block_during_shutdown_is_raised_at_the_request_end() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .rshutdown(freed_addref_at_rshutdown)
            .build("freed_use_at_end")])
        .build();
    let served = engine.serve_capture("<?php echo 'ran';", Capture::default());
    let stdout = served.stdout_text();
    assert!(stdout.starts_with(&format!("ran\n{REFUSED}")), "{stdout}");
    assert!(stdout.contains(SITE), "{stdout}");
    assert!(stdout.contains(" in Unknown on line 0\n"), "{stdout}");
    assert_eq!(served.exit_code, 255);
}

/// A request a sensed refusal has ended renders that one fatal alone: a
/// later refusal, here in a request-end function, is read through
/// memory the first has made untrustworthy, and is not raised (#2052).
#[test]
fn a_request_a_sensed_refusal_ended_renders_no_second() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .functions(vec![freed_addref::declared()])
            .rshutdown(freed_addref_at_rshutdown)
            .build("freed_use_twice")])
        .build();
    let served = engine.serve_capture(
        "<?php echo 'a';\nfreed_addref('string');\necho 'went on';\n",
        Capture::default(),
    );
    let stdout = served.stdout_text();
    assert!(stdout.starts_with(&format!("a\n{REFUSED}")), "{stdout}");
    assert_eq!(stdout.matches("InternalError").count(), 1, "{stdout}");
    assert_eq!(served.exit_code, 255);
}

/// A refusal made after a fatal, in a request-end function, is still
/// raised where the request ends: the fatal's `zend_bailout` was caught
/// before it, and a refused refcount change is a defect on any path
/// (#2048).
#[test]
fn an_addref_of_a_freed_block_after_a_fatal_is_raised_at_the_request_end() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .rshutdown(freed_addref_at_rshutdown)
            .build("freed_use_after_fatal")])
        .build();
    let served = engine.serve_capture(
        "<?php ini_set('memory_limit', '2M');\n$t = 'x'; for ($i = 0; $i < 16; $i++) { $t .= $t; }\n$s = '';\nwhile (true) { $s .= $t; }\n",
        Capture::default(),
    );
    let stdout = served.stdout_text();
    assert!(
        stdout.starts_with("\nFatal error: Allowed memory size of 2097152 bytes exhausted"),
        "{stdout}"
    );
    assert!(stdout.contains(REFUSED), "{stdout}");
    assert!(stdout.contains(" in Unknown on line 0\n"), "{stdout}");
    assert_eq!(served.exit_code, 255);
}

/// A request-start function as faulty, whose error then fails the start.
fn freed_addref_at_rinit(req: &mut Request) -> Result<(), Thrown> {
    if let Some(z) = freed(req, b"string") {
        addref_by_kind(z);
    }
    Err(crate::api::raising::throw_error(
        req,
        ClassId::ERROR,
        "start refused",
    ))
}

/// A refusal made where the request's start then failed, which serves no
/// handler and no request end, is raised where the start-failed request
/// ends, after the start's own fatal.
#[test]
fn an_addref_of_a_freed_block_before_a_failed_start_is_raised_where_it_ends() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .rinit(freed_addref_at_rinit)
            .build("freed_use_at_start")])
        .build();
    let mut sink = Capture::default();
    let observed = engine.serve_into(
        RequestDescription {
            sapi_name: String::from("cli"),
            script_name: Some(b"Standard input code".to_vec()),
            source: Some(b"<?php echo 'ran';".to_vec()),
            cwd: std::env::current_dir().expect("the working directory"),
            ..RequestDescription::default()
        },
        &mut sink,
    );
    let stdout = String::from_utf8_lossy(&sink.stdout);
    assert!(observed.start_failed);
    assert!(stdout.contains("Uncaught Error: start refused"), "{stdout}");
    assert!(stdout.contains(REFUSED), "{stdout}");
    assert!(stdout.contains(SITE), "{stdout}");
    assert!(!stdout.contains("ran"), "{stdout}");
}

/// The most recently freed slot of its bin is held back, so a stale
/// owner taken after a value of its size was made still names a freed
/// block and is refused the first time, for each of the five kinds.
#[test]
fn a_use_of_the_most_recently_freed_slot_is_refused_the_first_time() {
    let engine = Engine::cli_builder().extensions(&[faulty()]).build();
    for kind in ["string", "array", "object", "reference", "resource"] {
        let served = engine.serve_capture(
            &format!("<?php echo 'a';\nfreed_then_made('{kind}');\necho 'went on';\n"),
            Capture::default(),
        );
        let stdout = served.stdout_text();
        assert!(
            stdout.starts_with(&format!("a\n{REFUSED}")),
            "{kind}: {stdout}"
        );
        assert!(stdout.contains(SITE), "{kind}: {stdout}");
        assert!(!stdout.contains("went on"), "{kind}: {stdout}");
        assert_eq!(served.exit_code, 255, "{kind}");
    }
}

/// A write into a freed array its bin holds back is found as the block
/// leaves the quarantine: by `gc_mem_caches()`, whose sweep drains it
/// first, and by sixteen more arrays of its size freed after it. The
/// request goes on until then and ends in the `InternalError` naming the
/// block at the handler that evicted it.
#[test]
fn a_write_into_a_freed_block_is_found_as_it_leaves_the_quarantine() {
    let engine = Engine::cli_builder().extensions(&[faulty()]).build();
    for evict in [
        "gc_mem_caches();",
        "$a = []; for ($i = 0; $i < 20; $i++) { $a[] = [$i]; }\nunset($a);",
    ] {
        let served = engine.serve_capture(
            &format!("<?php freed_protected(); echo 'went on';\n{evict}\necho 'and on';\n"),
            Capture::default(),
        );
        let stdout = served.stdout_text();
        let (before, after) = stdout
            .split_once("-byte block at 0x")
            .unwrap_or_else(|| panic!("the fatal names the block: {stdout}"));
        assert!(
            before.starts_with("went on\nFatal error: InternalError: a write into the "),
            "{evict}: {stdout}"
        );
        assert!(
            after.contains(" after the heap freed it, found as it left the quarantine in "),
            "{evict}: {stdout}"
        );
        assert!(!stdout.contains("and on"), "{evict}: {stdout}");
        assert_eq!(served.exit_code, 255, "{evict}");
    }
}

/// A write into a freed block its bin still holds back when the request
/// ends is found there: the request leak sensor's sweep drains the
/// quarantine through the same check before it walks, and the write is
/// raised where the request ends, with nothing executing (#2053). A
/// clean request, its sensor armed as it is, reports nothing.
#[test]
fn a_write_into_a_freed_block_still_held_at_the_end_is_raised_there() {
    let engine = Engine::cli_builder().extensions(&[faulty()]).build();
    let served = engine.serve_capture(
        "<?php freed_protected(); echo 'went on';\n",
        Capture::default(),
    );
    let stdout = served.stdout_text();
    let (before, after) = stdout
        .split_once("-byte block at 0x")
        .unwrap_or_else(|| panic!("the fatal names the block: {stdout}"));
    assert!(
        before.starts_with("went on\nFatal error: InternalError: a write into the "),
        "{stdout}"
    );
    assert!(
        after.contains(
            " after the heap freed it, found as it left the quarantine in Unknown on line 0\n"
        ),
        "{stdout}"
    );
    assert_eq!(served.exit_code, 255);
    let clean = engine.serve_capture(
        "<?php $a = [1]; unset($a); echo 'clean';\n",
        Capture::default(),
    );
    clean.assert_clean("clean");
}

/// A read of a freed string's bytes is refused rather than aborting the
/// process on a slice the length of the freed-block mark: the native
/// reads no bytes, and the request ends in the `InternalError` naming
/// the block and the line that read (#2051).
#[test]
fn a_read_of_a_freed_strings_bytes_ends_the_request_and_names_the_line() {
    let engine = Engine::cli_builder().extensions(&[faulty()]).build();
    let served = engine.serve_capture(
        "<?php echo 'a';\necho freed_read();\necho 'went on';\n",
        Capture::default(),
    );
    let stdout = served.stdout_text();
    assert!(
        stdout.starts_with("a\nFatal error: InternalError: a read of the block at 0x"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            ", which the heap has freed, at crates/php_engine/src/tests/freed_use.rs:{READ_AT}:"
        )),
        "{stdout}"
    );
    assert!(stdout.contains(" on line 2\n"), "{stdout}");
    assert!(!stdout.contains("went on"), "{stdout}");
    assert_eq!(served.exit_code, 255);
}
