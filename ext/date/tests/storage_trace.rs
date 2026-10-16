//! Requested allocations around the existing date operations. The recorder
//! stores observations on the system heap; it neither serves nor charges PHP
//! allocations. Compare the receipt with the separately pinned zend-test
//! oracle, not with the main oracle's memory-usage measurements.
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
        let observer = req.observe_allocations(Box::new(Recorder::default()));
        req.state::<Trace>().active = Some(observer);
    } else if let Some(observer) = req.state::<Trace>().active.take() {
        let back: Option<Box<dyn Any>> = req.unobserve_allocations(observer).map(|o| o as _);
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

#[test]
fn requested_sizes_and_normalized_identities_are_retained_as_transition_evidence() {
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
    let served = engine.serve_capture(include_str!("storage/allocations.php"), Capture::default());
    assert_eq!(served.fatal, None);
    assert_eq!(served.exit_code, 0);
    let receipt = String::from_utf8_lossy(&served.stdout);
    assert!(receipt.starts_with("done\n"), "{receipt}");
    assert!(
        receipt
            .lines()
            .any(|line| line.starts_with("A ") && line.ends_with(" 240"))
    );
    assert!(
        receipt
            .lines()
            .any(|line| line.starts_with("A ") && line.ends_with(" 104"))
    );
    let oracle = include_str!("storage/allocations.oracle");
    let relative_allocations = |text: &str| {
        text.lines()
            .filter(|line| line.starts_with("A ") && line.ends_with(" 104"))
            .count()
    };
    // A retained counterexample, not the replacement's specification:
    // construction copies native relative state, frees it, then Held replays
    // its allocation. A real request-owned interval must remove this replay.
    assert_eq!(relative_allocations(oracle), 2);
    assert_eq!(relative_allocations(&receipt), 3);
    println!("relative allocations: oracle=2 transition=3; not allocation parity");
    println!("{receipt}");
}
