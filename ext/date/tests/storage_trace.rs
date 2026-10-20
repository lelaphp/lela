//! Requested allocations around the date operations, held to the receipt
//! of the separately pinned zend-test oracle, not to the main oracle's
//! memory-usage measurements. The recorder stores observations on the
//! system heap; it neither serves nor charges PHP allocations.
#![cfg(debug_assertions)]

use std::any::Any;
use std::fmt::Write as _;

use php_ext::abi::{ExtensionBuilder, IniStage};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::{Allocation, AllocationObserverId, ObserveAllocations, Request};
use php_ext::test::{Capture, Engine, LeakSensor};

const SWITCH: &[u8] = b"zend_test.zend_mm_custom_handlers.enabled";

#[derive(Default)]
struct Trace {
    active: Option<AllocationObserverId>,
    events: Vec<Allocation>,
}

#[derive(Default)]
struct Recorder(Vec<Allocation>);

impl ObserveAllocations for Recorder {
    fn observe(&mut self, what: Allocation) {
        self.0.push(what);
    }
}

#[expect(clippy::unnecessary_wraps, reason = "the ini handler's throwing shape")]
fn toggle(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    if req.ini_stage() == IniStage::Startup {
        return Ok(true);
    }
    if value.is_some_and(Request::ini_parse_bool) {
        let observer = req
            .memory_mut()
            .observe_allocations(Box::new(Recorder::default()));
        req.state::<Trace>().active = Some(observer);
    } else if let Some(observer) = req.state::<Trace>().active.take() {
        let back: Option<Box<dyn Any>> = req
            .memory_mut()
            .unobserve_allocations(observer)
            .map(|o| o as _);
        let recorder = back.and_then(|any| any.downcast::<Recorder>().ok());
        assert!(recorder.is_some());
        if let Some(recorder) = recorder {
            req.state::<Trace>().events = recorder.0;
        }
    }
    Ok(true)
}

fn finish(req: &mut Request) {
    let events = std::mem::take(&mut req.state::<Trace>().events);
    let mut live: Vec<(usize, usize)> = Vec::new();
    let mut next = 0;
    let mut receipt = String::new();
    for event in events {
        match event {
            Allocation::Allocated { at, size } => {
                next += 1;
                live.push((at, next));
                let _written = writeln!(receipt, "A {next} {size}");
            }
            Allocation::Freed { at } => {
                let position = live.iter().position(|(address, _)| *address == at);
                let identity = position.map_or(0, |position| live.remove(position).1);
                let _written = writeln!(receipt, "F {identity}");
            }
            Allocation::Reallocated { from, to, size } => {
                let position = live.iter().position(|(address, _)| *address == from);
                let identity = position.map_or(0, |position| {
                    live[position].0 = to;
                    live[position].1
                });
                let _written = writeln!(receipt, "R {identity} {size}");
            }
            Allocation::Collected { freed } => {
                let _written = writeln!(receipt, "C {freed}");
            }
        }
    }
    req.write(receipt.as_bytes());
}

/// The receipt of `source` served under the recorder, each line one
/// requested allocation or free between the switch's two writes.
fn receipt_of(source: &str) -> String {
    let observer = ExtensionBuilder::default()
        .request_state::<Trace>()
        .ini(vec![IniDecl {
            name: Name(SWITCH.into()),
            default: Some(b"0".as_slice().into()),
            user: true,
            per_dir: false,
            system: false,
        }])
        .on_modify(SWITCH, toggle)
        .rshutdown(finish)
        .build("storage_observer");
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension(), observer])
        .ini("date.timezone", "UTC")
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    assert_eq!(served.fatal, None);
    assert_eq!(served.exit_code, 0);
    let receipt = String::from_utf8_lossy(&served.stdout).into_owned();
    assert!(receipt.starts_with("done\n"), "{receipt}");
    println!("{receipt}");
    receipt
}

/// The lines of a receipt the two observers both see. The oracle's
/// `F 0` is a free of a block allocated before its observer was put in,
/// which the engine's observer does not see.
fn seen(receipt: &str) -> Vec<&str> {
    receipt
        .lines()
        .filter(|line| *line != "F 0" && *line != "done")
        .collect()
}

/// An interval's `timelib_rel_time` is the one timelib made, so the
/// constructor and the clone ask for the oracle's two 104-byte blocks,
/// in its order, and free them where it frees them (#2855).
#[test]
fn dates_zones_and_intervals_request_what_the_oracle_requests() {
    let receipt = receipt_of(include_str!("storage/allocations.php"));
    let oracle = include_str!("storage/allocations.oracle");
    assert_eq!(seen(&receipt), seen(oracle));
}

/// A period's `interval` is the `timelib_rel_time` timelib made, cloned
/// after its start and freed after its three times, as php's is.
#[test]
fn periods_request_what_the_oracle_requests() {
    let receipt = receipt_of(include_str!("storage/periods.php"));
    let oracle = include_str!("storage/periods.oracle");
    assert_eq!(seen(&receipt), seen(oracle));
}

/// An abbreviation zone's abbreviation is the `timelib_strdup` copy php's
/// `tzi.z.abbr` is, made where `timezone_initialize`, the clone and a
/// date's `getTimezone()` make it and freed where the zone is (#2885).
#[test]
fn abbreviation_zones_request_what_the_oracle_requests() {
    let receipt = receipt_of(include_str!("storage/abbreviations.php"));
    let oracle = include_str!("storage/abbreviations.oracle");
    assert_eq!(seen(&receipt), seen(oracle));
}
