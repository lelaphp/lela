//! Allocation observation as `TestExt` records it (design §7.1, as
//! #1780 ruled): an extension's observer told of the allocations, frees
//! and collections a request's heap serves while its entry is
//! on, the three acts `Z/ext/zend_test/tests/zend_mm_custom_handlers_{alloc,free,gc}_01.phpt`
//! drive, and the heap's readings what they are with no observer.

use super::test_ext::{Event, Events, Phase, Recording, TestExt};
use crate::api::request::Allocation;
use crate::engine::Engine;
use crate::test::Capture;

/// What extension `n` was told of the heap, in order.
fn told(events: &Events, n: u8) -> Vec<Allocation> {
    events
        .0
        .iter()
        .filter_map(|e| match e {
            Event::Ext(at, Phase::Allocation(what)) if *at == n => Some(*what),
            _ => None,
        })
        .collect()
}

/// `source` served by an engine whose one extension observes
/// allocations, and what it was told while serving it.
fn observed(source: &str) -> (String, Vec<Allocation>) {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing_allocations()])
        .build();
    rec.mark("built");
    let served = engine.serve_capture(source, Capture::default());
    rec.mark("served");
    let stdout = served.stdout_text();
    (stdout, told(&rec.events().between("built", "served"), 0))
}

/// `zend_mm_custom_handlers_alloc_01.phpt`, the string made by a rope
/// since `str_repeat` is `standard`'s: a string made while the entry is
/// on is told, at the size the string asked the heap for, php's
/// `ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(60))`; strings of that size
/// made before the entry is turned on or after it is turned off are
/// not told.
#[test]
fn a_string_made_while_observed_is_told_at_the_size_it_asked_for() {
    let (stdout, told) = observed(
        "<?php\n\
         $p = 'StringStri';\n\
         $before = \"$p$p$p$p$p$p\";\n\
         ini_set('test_ext_0.observe_allocations', 1);\n\
         $string = \"$p$p$p$p$p$p\";\n\
         ini_set('test_ext_0.observe_allocations', 0);\n\
         $after = \"$p$p$p$p$p$p\";\n\
         echo strlen($string);\n",
    );
    assert_eq!(stdout, "60");
    let sized_88 = told
        .iter()
        .filter(|what| matches!(what, Allocation::Allocated { size: 88, .. }))
        .count();
    assert_eq!(sized_88, 1, "told {told:?}");
}

/// `zend_mm_custom_handlers_free_01.phpt`: a string made before the
/// entry is turned on and unset while it is on is told as freed, at the
/// address no allocation it was told of answered.
#[test]
fn a_string_unset_while_observed_is_told_freed() {
    let (_, told) = observed(
        "<?php\n\
         $p = 'StringStri';\n\
         $string = \"$p$p$p$p$p$p\";\n\
         ini_set('test_ext_0.observe_allocations', 1);\n\
         unset($string);\n\
         ini_set('test_ext_0.observe_allocations', 0);\n",
    );
    let allocated: Vec<usize> = told
        .iter()
        .filter_map(|what| match what {
            Allocation::Allocated { at, .. } => Some(*at),
            _ => None,
        })
        .collect();
    let freed_before = told
        .iter()
        .any(|what| matches!(what, Allocation::Freed { at } if !allocated.contains(at)));
    assert!(freed_before, "told {told:?}");
}

/// `zend_mm_custom_handlers_gc_01.phpt`: `gc_mem_caches()` while the
/// entry is on is told as a collection, with the bytes it gave back,
/// which is what the native answered.
#[test]
fn gc_mem_caches_while_observed_is_told_with_what_it_gave_back() {
    let (stdout, told) = observed(
        "<?php\n\
         ini_set('test_ext_0.observe_allocations', 1);\n\
         $freed = gc_mem_caches();\n\
         ini_set('test_ext_0.observe_allocations', 0);\n\
         echo $freed;\n",
    );
    let collected: Vec<String> = told
        .iter()
        .filter_map(|what| match what {
            Allocation::Collected { freed } => Some(freed.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(collected, [stdout]);
}

/// What `memory_get_usage()` and its peak read across a program's
/// allocations, small, large and huge, grown and freed: the same with no
/// observer declared, with one declared and its entry off, and with its
/// entry on, since the heap serves each allocation whether or not it is
/// observed and observing allocates nothing of the request's. The
/// declared extension's own footprint at the request's start is the
/// same whether its entry is on or off.
#[test]
fn what_memory_get_usage_reads_is_the_same_observed_or_not() {
    let source = |on: u8| {
        format!(
            "<?php\n\
             $start = memory_get_usage();\n\
             if (ini_get('test_ext_0.observe_allocations') !== false) {{ ini_set('test_ext_0.observe_allocations', {on}); }}\n\
             $m = memory_get_usage(); memory_reset_peak_usage();\n\
             $a = []; for ($i = 0; $i < 1000; $i++) {{ $a[] = \"x$i\"; }}\n\
             $c = 'y'; for ($i = 0; $i < 10; $i++) {{ $c .= $c; }}\n\
             $s = ''; for ($i = 0; $i < 3000; $i++) {{ $s .= $c; }}\n\
             unset($a); $s .= 'z';\n\
             echo memory_get_usage() - $m, ' ', memory_get_peak_usage() - $m, ' ', memory_get_usage(true), ' ', strlen($s), '|', $start;\n"
        )
    };
    let _rec = Recording::start();
    let plain = Engine::cli_builder().build();
    let declared = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing_allocations()])
        .build();
    let read = |engine: &Engine, on: u8| {
        let served = engine.serve_capture(&source(on), Capture::default());
        let stdout = served.stdout_text();
        let (read, start) = stdout.split_once('|').unwrap_or((&stdout, ""));
        (read.to_owned(), start.to_owned())
    };
    let (none, _) = read(&plain, 0);
    assert!(none.ends_with(" 3072001"), "read {none}");
    let (off, start_off) = read(&declared, 0);
    let (on, start_on) = read(&declared, 1);
    assert_eq!(off, none);
    assert_eq!(on, none);
    assert_eq!(start_on, start_off);
}
