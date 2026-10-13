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

#[test]
fn current_system_pool_cannot_prove_php_allocation_parity() {
    let bytes = served(include_str!("proof/allocation.php"));
    assert_ne!(
        bytes,
        include_bytes!("../../../data/uri-storage-2832/allocation.stdout")
    );
    // Keep the falsification specific: a large URL causes request-pool growth
    // in php, while the current binding charges only its PHP object.
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("pool-grow 80\n"), "{text}");
    assert!(text.contains("pool-retain -80\n"), "{text}");
}
