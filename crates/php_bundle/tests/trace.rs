//! The witness runner's trace (harness §4.5 condition 2, revision 58),
//! held over a real expansion: the parity extension's entries, built by
//! `php_macros`, record themselves as the request enters them, named by
//! this crate, their `#[php_class]` type and their identifier, which is
//! what `xtask witness` reads coverage by. A constructor php reaches
//! through `new` alone is recorded; a method the program names but never
//! reaches is not; and a request served without the trace records nothing.

#[path = "parity/extension.rs"]
mod extension;

use php_ext::test::{Capture, Engine, Entered};

const SCRIPT: &str = "<?php $b = new ProtoBox(1); $b->grow(); echo proto_count([1, 2]), \"\\n\"; throw new Exception('first'); $b->value();";

fn engine() -> Engine {
    Engine::builder()
        .extensions(&[extension::extension()])
        .build()
}

/// The natives of this crate a request entered, as `(owner, name)`.
fn ours(entered: &[Entered]) -> Vec<(Option<&'static str>, &'static str)> {
    entered
        .iter()
        .filter(|e| e.krate == env!("CARGO_CRATE_NAME"))
        .map(|e| (e.owner, e.name))
        .collect()
}

#[test]
fn a_traced_request_records_what_it_entered_and_nothing_it_only_named() {
    // The traced face serves a file, as the witness lane writes its fence.
    let dir = std::env::temp_dir().join(format!("php-bundle-trace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("trace.php"), SCRIPT).unwrap();
    let served =
        engine().serve_capture_traced_in(&dir, b"trace.php", Vec::new(), Capture::default());
    std::fs::remove_dir_all(&dir).ok();
    let entered = ours(&served.entered);
    assert!(
        entered.contains(&(Some("ProtoBox"), "__construct")),
        "the constructor, reached through `new` alone: {entered:?}"
    );
    assert!(entered.contains(&(Some("ProtoBox"), "grow")), "{entered:?}");
    assert!(entered.contains(&(None, "proto_count")), "{entered:?}");
    assert!(
        !entered.contains(&(Some("ProtoBox"), "value")),
        "named after the throw and never reached: {entered:?}"
    );
}

#[test]
fn a_request_served_without_the_trace_records_nothing() {
    let dir = std::env::temp_dir();
    let served = engine().serve_capture_in(&dir, b"trace.php", SCRIPT, Capture::default());
    assert!(served.entered.is_empty(), "{:?}", served.entered);
}

/// A request that calls one native many times records it once: the trace
/// is the set of natives the request entered, in the order it first
/// entered each, not a line per call.
#[test]
fn a_traced_request_records_a_native_it_calls_many_times_once() {
    let dir = std::env::temp_dir().join(format!("php-bundle-trace-loop-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("loop.php"),
        "<?php for ($i = 0; $i < 100; $i++) { proto_count([1]); } echo proto_count([1, 2]), \"\\n\";",
    )
    .unwrap();
    let served =
        engine().serve_capture_traced_in(&dir, b"loop.php", Vec::new(), Capture::default());
    std::fs::remove_dir_all(&dir).ok();
    let entered = ours(&served.entered);
    let times = entered
        .iter()
        .filter(|e| **e == (None, "proto_count"))
        .count();
    assert_eq!(times, 1, "{entered:?}");
}
