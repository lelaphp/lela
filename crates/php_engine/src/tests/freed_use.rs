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
            let kind = req.resources().register_type(b"freed", None);
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

fn faulty() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
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
        let stdout = String::from_utf8_lossy(&served.stdout);
        assert!(
            stdout.starts_with(&format!("a\n{REFUSED}")),
            "{kind}: {stdout}"
        );
        assert!(stdout.contains(SITE), "{kind}: {stdout}");
        // The loop asks after the native's handler, which has moved
        // the frame to the next opline, so that is the line rendered.
        assert!(stdout.contains(" on line 3\n"), "{kind}: {stdout}");
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
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(stdout.starts_with(&format!("ran\n{REFUSED}")), "{stdout}");
    assert!(stdout.contains(SITE), "{stdout}");
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
        "Error",
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
    let observed = engine.serve(
        RequestDescription {
            sapi_name: String::from("cli"),
            script_name: Some(b"Standard input code".to_vec()),
            source: Some(b"<?php echo 'ran';".to_vec()),
            cwd: std::env::current_dir().unwrap_or_default(),
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
        let stdout = String::from_utf8_lossy(&served.stdout);
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
        let stdout = String::from_utf8_lossy(&served.stdout);
        let (before, after) = stdout.split_once("-byte block at 0x").unwrap_or_default();
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
