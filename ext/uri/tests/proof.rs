//! Retained bounded PHP transactions from #2832 through the existing Capture
//! seam. Expected bytes are collected from the pinned oracle by
//! data/uri-storage-2832/collect.py, not derived from this engine.

use php_ext::test::{Capture, Engine};

fn served(source: &str) -> Vec<u8> {
    let engine = Engine::builder()
        .extensions(&[php_ext_lexbor::extension(), php_ext_uri::extension()])
        .build();
    engine.serve_capture(source, Capture::default()).stdout
}

#[test]
fn pinned_standards_and_read_order() {
    assert_eq!(
        served(include_str!("proof/semantics.php")),
        include_bytes!("../../../data/uri-storage-2832/semantics.stdout")
    );
}

#[test]
fn immutable_updates_include_successful_no_ops_and_refusals() {
    assert_eq!(
        served(include_str!("proof/updates.php")),
        include_bytes!("../../../data/uri-storage-2832/updates.stdout")
    );
}

#[test]
fn transformed_and_nul_contexts_outlive_parser_scratch() {
    assert_eq!(
        served(include_str!("proof/diagnostics.php")),
        include_bytes!("../../../data/uri-storage-2832/diagnostics.stdout")
    );
}

#[test]
fn errors_reference_assignment_reenters_without_losing_parse_state() {
    assert_eq!(
        served(include_str!("proof/reentry.php")),
        include_bytes!("../../../data/uri-storage-2832/reentry.stdout")
    );
}

/// The request's pool is lexbor's on the request's heap (#2925), so a large
/// URL grows it as php's grows, and the allocation transactions read the
/// oracle's.
#[test]
fn the_request_s_pool_allocates_as_php_s() {
    assert_eq!(
        served(include_str!("proof/allocation.php")),
        include_bytes!("../../../data/uri-storage-2832/allocation.stdout")
    );
}
