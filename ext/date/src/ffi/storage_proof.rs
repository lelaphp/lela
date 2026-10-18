//! Bounded, test-only storage probes; no ordinary date path calls these.
//! Byte records contain the values being read, not a compatibility charge.
//! The direct existing-state and two-word block payloads are falsified below.
//! This is not a timezone decoder, parser replacement or approved interface.

use std::any::Any;
use std::mem::{align_of, offset_of, size_of};

use php_ext::abi::ExtensionBuilder;
use php_ext::raising::Thrown;
use php_ext::request::{Allocation, ObserveAllocations, Request};
use php_ext::test::{Capture, Engine, LeakSensor};
use php_ext::values::{Emalloc, Owner, PhpArray};

use super::super::{RelFields, Time, TimeFields};
use crate::State;
use crate::lent::Lent;
use crate::obj::{DateState, IntervalState, PeriodState, TzState};

#[derive(Default)]
struct Recorder(Vec<Allocation>);

// A full direct owner using the existing safe block shape in every native
// pointer slot. The measured failure rules out placing this in the 176-byte
// zone allocation; merely replacing the pointers does not preserve the size.
#[repr(C)]
struct OwnedZoneShape {
    name: Option<Emalloc>,
    counts32: [u32; 6],
    counts64: [u64; 6],
    transitions: Option<Emalloc>,
    indexes: Option<Emalloc>,
    types: Option<Emalloc>,
    abbreviations: Option<Emalloc>,
    leaps: Option<Emalloc>,
    backward_compatibility: u8,
    country: [u8; 3],
    latitude: f64,
    longitude: f64,
    comments: Option<Emalloc>,
    posix_text: Option<Emalloc>,
    posix_rules: Option<Emalloc>,
}

impl ObserveAllocations for Recorder {
    fn observe(&mut self, what: Allocation) {
        self.0.push(what);
    }
}

fn record(req: &mut Request, size: usize) -> Result<Emalloc, Thrown> {
    let mut block = Emalloc::new(req, size)?;
    assert_eq!(block.len(), size);
    assert!(block.bytes().iter().all(|byte| *byte == 0));
    assert_eq!(block.as_mut_ptr() as usize % 8, 0);
    Ok(block)
}

fn put(block: &mut Emalloc, at: usize, bytes: &[u8]) {
    block.bytes_mut()[at..at + bytes.len()].copy_from_slice(bytes);
}

fn relative(req: &mut Request, value: &RelFields) -> Result<Emalloc, Thrown> {
    let mut block = record(req, size_of::<RelFields>())?;
    macro_rules! fields {
        ($($field:ident),+ $(,)?) => {
            $(put(&mut block, offset_of!(RelFields, $field), &value.$field.to_ne_bytes());)+
        };
    }
    fields!(
        y,
        m,
        d,
        h,
        i,
        s,
        us,
        weekday,
        weekday_behavior,
        first_last_day_of,
        invert,
        days,
        special_type,
        special_amount,
        have_weekday_relative,
        have_special_relative
    );
    Ok(block)
}

fn read_i64(block: &Emalloc, at: usize) -> i64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&block.bytes()[at..at + 8]);
    i64::from_ne_bytes(bytes)
}

fn bytes(req: &mut Request, value: &[u8]) -> Result<Emalloc, Thrown> {
    let mut block = record(req, value.len() + 1)?;
    put(&mut block, 0, value);
    Ok(block)
}

fn copy(req: &mut Request, source: &Emalloc) -> Result<Emalloc, Thrown> {
    let mut block = record(req, source.len())?;
    block.bytes_mut().copy_from_slice(source.bytes());
    Ok(block)
}

fn serve(
    hook: fn(&mut Request) -> Result<(), Thrown>,
    limit: &str,
) -> (Option<String>, i32, Vec<u8>) {
    let engine = Engine::builder()
        .extensions(&[ExtensionBuilder::default()
            .rinit(hook)
            .build("storage_probe")])
        .ini("memory_limit", limit)
        .ini("html_errors", "0")
        .ini("log_errors", "0")
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture("<?php", Capture::default());
    (served.fatal, served.exit_code, served.stdout)
}

#[test]
fn existing_owned_states_and_safe_block_payloads_do_not_fit_php_objects() {
    // The custom region is PHP's declared size minus its 56-byte header.
    // Larger payloads silently enlarge allocation_size, so they are rejected.
    for (name, size, alignment, room) in [
        ("date", size_of::<DateState>(), align_of::<DateState>(), 8),
        (
            "interval",
            size_of::<IntervalState>(),
            align_of::<IntervalState>(),
            24,
        ),
        (
            "period",
            size_of::<PeriodState>(),
            align_of::<PeriodState>(),
            48,
        ),
    ] {
        println!(
            "{name}: size={size} align={alignment} custom={room} rejected={}",
            size > room
        );
        assert!(size > room);
        assert!(alignment <= 8);
    }
    // A `DateTimeZone`'s state lies inside its object since #2630.
    assert!(size_of::<TzState>() <= 32 && align_of::<TzState>() <= 8);
    println!(
        "time={} relative={} zone={} diagnostic={} message={} block={} optional-block={}",
        size_of::<TimeFields>(),
        size_of::<RelFields>(),
        size_of::<timelib_sys::TzInfo>(),
        size_of::<timelib_sys::ErrorContainer>(),
        size_of::<timelib_sys::ErrorMessage>(),
        size_of::<Emalloc>(),
        size_of::<Option<Emalloc>>()
    );
    assert_eq!(size_of::<RelFields>(), crate::held::REL_TIME);
    assert!(size_of::<Option<Emalloc>>() > 8);
    println!(
        "direct owned-zone shape={} native-zone={}: rejected",
        size_of::<OwnedZoneShape>(),
        size_of::<timelib_sys::TzInfo>()
    );
    assert!(size_of::<OwnedZoneShape>() > size_of::<timelib_sys::TzInfo>());
}

fn real_records(req: &mut Request) -> Result<(), Thrown> {
    let before = req.memory_usage(false);
    let observer = req.observe_allocations(Box::new(Recorder::default()));
    let value = RelFields {
        y: -2,
        d: 3,
        h: 4,
        us: 123_456,
        days: 731,
        special_amount: -17,
        have_special_relative: 1,
        ..RelFields::default()
    };
    let interval = relative(req, &value)?;
    let cloned = copy(req, &interval)?;
    assert_eq!(read_i64(&cloned, offset_of!(RelFields, days)), 731);
    assert_eq!(
        read_i64(&cloned, offset_of!(RelFields, special_amount)),
        -17
    );
    let abbreviation = bytes(req, b"EST")?;
    assert_eq!(abbreviation.bytes(), b"EST\0");
    let cloned_abbreviation = copy(req, &abbreviation)?;
    interval.free(req);
    abbreviation.free(req);
    assert_eq!(cloned_abbreviation.bytes(), b"EST\0");
    cloned.free(req);
    cloned_abbreviation.free(req);

    // Ordered diagnostics: position, character, code and private text index
    // are stored in the record; the corresponding text is the actual block.
    let mut message = record(req, size_of::<timelib_sys::ErrorMessage>())?;
    put(&mut message, 0, &7_i32.to_ne_bytes());
    put(&mut message, 4, &13_i32.to_ne_bytes());
    put(&mut message, 8, b"!");
    put(&mut message, 16, &1_u64.to_ne_bytes());
    let text = bytes(req, b"Unexpected character")?;
    assert_eq!(message.bytes()[8], b'!');
    assert_eq!(text.bytes(), b"Unexpected character\0");
    let mut container = record(req, size_of::<timelib_sys::ErrorContainer>())?;
    put(&mut container, 0, &1_u64.to_ne_bytes());
    put(&mut container, 16, &1_i32.to_ne_bytes());
    container.free(req);
    message.free(req);
    text.free(req);
    let back: Option<Box<dyn Any>> = req.unobserve_allocations(observer).map(|o| o as _);
    let events = back.and_then(|any| any.downcast::<Recorder>().ok());
    assert!(events.is_some());
    if let Some(events) = events {
        let sizes: Vec<_> = events
            .0
            .iter()
            .filter_map(|event| match event {
                Allocation::Allocated { size, .. } => Some(*size),
                _ => None,
            })
            .collect();
        assert_eq!(sizes, [104, 104, 4, 4, 24, 21, 24]);
        assert_eq!(
            events
                .0
                .iter()
                .filter(|e| matches!(e, Allocation::Freed { .. }))
                .count(),
            7
        );
        println!("actual record allocations: {sizes:?}; usage returned to baseline");
    }
    assert_eq!(req.memory_usage(false), before);
    Ok(())
}

#[test]
fn actual_relative_abbreviation_and_diagnostic_records_clone_and_release() {
    let served = serve(real_records, "128M");
    assert_eq!(served.0, None);
    assert_eq!(served.1, 0);
}

fn real_date(req: &mut Request) -> Result<(), Thrown> {
    let before = req.memory_usage(false);
    let mut block = record(req, size_of::<TimeFields>())?;
    {
        // Native parsing is only the independent source of values for this
        // storage probe; the detached allocations are system input, not the
        // candidate's authoritative storage or allocation evidence.
        let mut state = State::default();
        let mut lent = Lent::detached(&mut state);
        let parsed = Time::parse_from_format(&mut lent, b"!Y-m-d H:i:s", b"2024-02-29 12:34:56");
        macro_rules! fields {
            ($($field:ident),+ $(,)?) => {
                $(put(&mut block, offset_of!(TimeFields, $field), &parsed.time.$field.to_ne_bytes());)+
            };
        }
        fields!(
            y,
            m,
            d,
            h,
            i,
            s,
            us,
            z,
            dst,
            sse,
            have_time,
            have_date,
            have_zone,
            have_relative,
            have_weeknr_day,
            sse_uptodate,
            tim_uptodate,
            is_localtime,
            zone_type
        );
        let rel = relative(req, &parsed.time.relative)?;
        put(&mut block, offset_of!(TimeFields, relative), rel.bytes());
        rel.free(req);
        // No abbreviation or zone for this parse-only UTC input; the two
        // zero association words name none, never retain native pointers.
    }
    assert_eq!(read_i64(&block, offset_of!(TimeFields, y)), 2024);
    assert_eq!(read_i64(&block, offset_of!(TimeFields, m)), 2);
    assert_eq!(read_i64(&block, offset_of!(TimeFields, d)), 29);
    assert_eq!(read_i64(&block, offset_of!(TimeFields, s)), 56);
    let cloned = copy(req, &block)?;
    block.free(req);
    assert_eq!(read_i64(&cloned, offset_of!(TimeFields, d)), 29);
    cloned.free(req);
    assert_eq!(req.memory_usage(false), before);
    Ok(())
}

#[test]
fn date_bytes_remain_the_storage_after_the_source_time_is_destroyed() {
    let served = serve(real_date, "128M");
    assert_eq!(served.0, None);
}

fn cache(req: &mut Request) -> Result<(), Thrown> {
    let before = req.memory_usage(false);
    let mut table = PhpArray::new(req)?;
    for (index, name) in [
        "Europe/Amsterdam",
        "America/New_York",
        "Asia/Tokyo",
        "Europe/London",
        "Australia/Sydney",
        "Africa/Cairo",
        "America/Chicago",
        "Europe/Paris",
        "Asia/Kolkata",
    ]
    .iter()
    .enumerate()
    {
        table.set_str(
            req,
            name.as_bytes(),
            Owner::int(i64::try_from(index).unwrap_or_default()),
        )?;
    }
    assert_eq!(table.len(), 9);
    table.set_str(req, b"Europe/Amsterdam", Owner::int(0))?;
    assert_eq!(table.len(), 9);
    // The key and bucket storage is the request array itself, not a HashMap
    // beside representative blocks. These private numbers name zones only.
    let transitions = {
        let mut state = State::default();
        let mut lent = Lent::detached(&mut state);
        let zone = lent.zone(b"Europe/Amsterdam");
        assert!(zone.is_some());
        zone.map(|zone| zone.transitions()).unwrap_or_default()
    };
    assert!(transitions.len() > 3);
    let mut payload = record(req, 8 * transitions.len())?;
    let mut indexes = record(req, transitions.len())?;
    for (index, (timestamp, kind)) in transitions.iter().enumerate() {
        put(&mut payload, index * 8, &timestamp.to_ne_bytes());
        indexes.bytes_mut()[index] = u8::try_from(*kind).unwrap_or_default();
    }
    let old_len = payload.len();
    let payload = payload.resize(req, old_len + 24)?;
    assert_eq!(read_i64(&payload, 16), transitions[2].0);
    assert!(payload.bytes()[old_len..].iter().all(|byte| *byte == 0));
    indexes.free(req);
    payload.free(req);
    table.give_up(req);
    assert_eq!(req.memory_usage(false), before);
    Ok(())
}

#[test]
fn actual_request_cache_buckets_and_transition_bytes_grow_and_release() {
    let served = serve(cache, "128M");
    assert_eq!(served.0, None);
}

fn refusal(req: &mut Request) -> Result<(), Thrown> {
    let prefix = bytes(req, b"partially initialized zone")?;
    match Emalloc::new(req, 64 * 1024 * 1024) {
        Ok(block) => {
            block.free(req);
            prefix.free(req);
            Ok(())
        }
        Err(thrown) => {
            prefix.free(req);
            Err(thrown)
        }
    }
}

#[test]
fn real_memory_limit_refusal_releases_the_initialized_prefix() {
    let served = serve(refusal, "2M");
    assert!(
        served
            .0
            .as_ref()
            .is_some_and(|fatal| fatal.contains("Allowed memory size")),
        "{served:?}"
    );
    assert!(!String::from_utf8_lossy(&served.2).contains("InternalError"));
}

fn prefixes(req: &mut Request) -> Result<(), Thrown> {
    let before = req.memory_usage(false);
    for fail_after in 0..=6 {
        let mut initialized = Vec::new();
        for size in [176, 17, 24, 3, 60, 8].into_iter().take(fail_after) {
            let mut block = record(req, size)?;
            block.bytes_mut().fill(0x5a);
            initialized.push(block);
        }
        for block in initialized {
            block.free(req);
        }
        assert_eq!(req.memory_usage(false), before);
    }
    Ok(())
}

#[test]
fn each_initialized_prefix_has_individual_cleanup_before_request_teardown() {
    let served = serve(prefixes, "128M");
    assert_eq!(served.0, None);
}
