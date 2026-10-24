//! The request's storage as a native holds it (design §5.13): an
//! [`Emalloc`] is `ecalloc`'s bytes on the request's heap, zero and
//! eight-aligned, counted by `memory_get_usage()` at its bin, kept to the
//! shorter length by a resize, given back by its free or by the request's
//! end, named by an armed leak sensor where its holder forgot it, and
//! refused by `memory_limit` with the limit's fatal; a [`FreshString`]
//! grows as `erealloc` grows a string, and keeps its allocation where its
//! length is cut in place, as `ZSTR_LEN` is, and moves to the smaller bin
//! where `zend_string_truncate` cuts it.

use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Emalloc, FreshString, Owner, PhpString};
use crate::engine::{Engine, LeakSensor};
use crate::test::{Capture, Served};

/// What the request's heap counts, as `memory_get_usage()` reads it.
fn used(req: &mut Request) -> i64 {
    i64::try_from(req.memory().usage(false)).unwrap_or(i64::MAX)
}

/// `$len` as the length a native asks for; a negative one asks for
/// none.
fn length(len: i64) -> usize {
    usize::try_from(len).unwrap_or(0)
}

/// The byte a test writes at `at`: never zero, so a byte a resize zeroes
/// is not mistaken for one it kept.
fn written(at: usize) -> u8 {
    u8::try_from(at % 251).unwrap_or(0) + 1
}

/// `block_held(int $len): string`: one block of `$len` bytes made and
/// freed; what the heap counted while it was held, whether its bytes
/// read zero and its first byte is eight-aligned, and what the heap
/// counts after its free.
#[php_function]
fn block_held(req: &mut Request, len: i64) -> Result<Owner<PhpString>, Thrown> {
    let before = used(req);
    let mut block = Emalloc::new(req, length(len))?;
    let held = used(req) - before;
    let zero = block.bytes().iter().all(|&byte| byte == 0);
    let aligned = (block.as_mut_ptr() as usize).is_multiple_of(8);
    block.free(req);
    let freed = used(req) - before;
    PhpString::new(req, format!("{held} {zero} {aligned} {freed}").as_bytes())
}

/// `block_resized(int $from, int $to): string`: a block of `$from`
/// bytes, written, resized to `$to`; its length, whether the shorter of
/// the two lengths' bytes are the ones written and the bytes past them
/// zero, and what the heap counted while the resized block was held.
#[php_function]
fn block_resized(req: &mut Request, from: i64, to: i64) -> Result<Owner<PhpString>, Thrown> {
    let (from, to) = (length(from), length(to));
    let before = used(req);
    let mut block = Emalloc::new(req, from)?;
    for (at, byte) in block.bytes_mut().iter_mut().enumerate() {
        *byte = written(at);
    }
    let block = block.resize(req, to)?;
    let kept = from.min(to);
    let prefix = block.bytes()[..kept]
        .iter()
        .enumerate()
        .all(|(at, &byte)| byte == written(at));
    let zero = block.bytes()[kept..].iter().all(|&byte| byte == 0);
    let held = used(req) - before;
    let len = block.len();
    block.free(req);
    PhpString::new(req, format!("{len} {prefix} {zero} {held}").as_bytes())
}

/// The blocks a request keeps to its end, freed by [`kept_freed`].
#[derive(Default)]
struct Kept(Vec<Emalloc>);

/// The request-end function that frees what [`Kept`] holds, with the
/// state's borrow ended before the request frees.
fn kept_freed(req: &mut Request) {
    for block in std::mem::take(&mut req.state::<Kept>().0) {
        block.free(req);
    }
}

/// `block_kept(string $how): void`: a 24-byte block made and kept in
/// the request's state to its end (`request`), forgotten, with no free
/// at all (`forget`), or freed now (any other word).
#[php_function]
fn block_kept(req: &mut Request, how: PhpString) -> Result<(), Thrown> {
    let block = Emalloc::new(req, 24)?;
    match how.as_bytes() {
        b"request" => req.state::<Kept>().0.push(block),
        // The block goes out of scope with no free.
        b"forget" => {}
        _ => block.free(req),
    }
    Ok(())
}

/// `block_refused(int $len): void`: a 24-byte block made and written,
/// then a block of `$len` asked for; where the limit refuses it the
/// first is freed before the fatal goes on.
#[php_function]
fn block_refused(req: &mut Request, len: i64) -> Result<(), Thrown> {
    let mut first = Emalloc::new(req, 24)?;
    first.bytes_mut().fill(0x5a);
    let second = Emalloc::new(req, length(len));
    first.free(req);
    second.map(|block| block.free(req))
}

/// `string_grown(): string`: a fresh string of 2050 bytes, extended to
/// 3075 and then 4612, as libmbfl's buffer grows by half again of what
/// it holds, and its length set to 4100; what the heap counted at each
/// step, and after the string handed out is given up.
#[php_function]
fn string_grown(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let before = used(req);
    let mut text = FreshString::new(req, 2050)?;
    let made = used(req) - before;
    text = text.extend(req, 3075)?;
    let first = used(req) - before;
    text = text.extend(req, 4612)?;
    let second = used(req) - before;
    text.bytes_mut()[..4100].fill(b'a');
    text.set_len(4100);
    let set = used(req) - before;
    text.finish().give_up(req);
    let given_up = used(req) - before;
    PhpString::new(
        req,
        format!("{made} {first} {second} {set} {given_up}").as_bytes(),
    )
}

/// `string_cut(bool $truncate): string`: a fresh string of 2050 bytes
/// written, extended to 3550, and cut to its first seven bytes, by
/// `zend_string_truncate` where `$truncate` and by its length alone
/// otherwise; what the heap counted after the extension and after the
/// cut, whether the seven bytes are the ones written, and what it counts
/// after the string handed out is given up.
#[php_function]
fn string_cut(req: &mut Request, truncate: bool) -> Result<Owner<PhpString>, Thrown> {
    let before = used(req);
    let mut text = FreshString::new(req, 2050)?;
    text.bytes_mut()[..7].copy_from_slice(b"payload");
    text = text.extend(req, 3550)?;
    let grown = used(req) - before;
    if truncate {
        text = text.truncate(req, 7)?;
    } else {
        text.set_len(7);
    }
    let cut = used(req) - before;
    let finished = text.finish();
    let kept = finished.as_bytes() == b"payload";
    finished.give_up(req);
    let given_up = used(req) - before;
    PhpString::new(req, format!("{grown} {cut} {kept} {given_up}").as_bytes())
}

/// `string_short(): string`: what the heap counts for a fresh string of
/// no bytes and one of one byte, each handed out and given up: a block
/// of its own each time, never the interned empty or one-character
/// string, as `zend_string_alloc` makes no interned string.
#[php_function]
fn string_short(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let before = used(req);
    let empty = FreshString::new(req, 0)?.finish();
    let held_empty = used(req) - before;
    empty.give_up(req);
    let one = FreshString::new(req, 1)?.finish();
    let held_one = used(req) - before;
    one.give_up(req);
    PhpString::new(req, format!("{held_empty} {held_one}").as_bytes())
}

/// `string_refused(int $len, int $to): void`: a fresh string of `$len`
/// bytes, extended to `$to` where `$to` is longer.
#[php_function]
fn string_refused(req: &mut Request, len: i64, to: i64) -> Result<(), Thrown> {
    let mut text = FreshString::new(req, length(len))?;
    if to > len {
        text = text.extend(req, length(to))?;
    }
    text.free(req);
    Ok(())
}

fn storage() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            block_held::declared(),
            block_resized::declared(),
            block_kept::declared(),
            block_refused::declared(),
            string_grown::declared(),
            string_cut::declared(),
            string_short::declared(),
            string_refused::declared(),
        ])
        .request_state::<Kept>()
        .rshutdown(kept_freed)
        .build("storage")
}

/// A CLI engine with [`storage`], the leak sensor `sensor` and, where
/// one is named, `memory_limit` at `limit`.
fn engine(sensor: LeakSensor, limit: Option<&str>) -> Engine {
    let builder = Engine::cli_builder()
        .extensions(&[storage()])
        .sense_leaks(sensor);
    match limit {
        Some(limit) => builder.ini("memory_limit", limit),
        None => builder,
    }
    .build()
}

/// `src` served by [`engine`] with the leak sensor `sensor`.
fn served(sensor: LeakSensor, src: &str) -> Served {
    engine(sensor, None).serve_capture(src, Capture::default())
}

/// What a clean request served with the sensor armed wrote.
fn clean(src: &str) -> String {
    let served = served(LeakSensor::Armed, src);
    let stdout = served.stdout_text();
    assert_eq!(
        (served.exit_code, served.fatal.as_deref()),
        (0, None),
        "{stdout}"
    );
    stdout
}

/// A block is counted at its bin, a small one's size class, sixteen
/// bytes at least as `ZEND_MM_HEAP_PROTECTION` asks, or a large one's
/// pages; it reads zero, starts eight-aligned, and its free gives back
/// all of it.
#[test]
fn a_block_is_zero_aligned_and_counted_at_its_bin() {
    assert_eq!(
        clean("<?php foreach ([1, 24, 104, 3000, 5000] as $len) echo block_held($len), \"\\n\";"),
        "16 true true 0\n24 true true 0\n112 true true 0\n3072 true true 0\n8192 true true 0\n"
    );
}

/// A resize keeps the bytes up to the shorter length and zeroes the
/// rest, grown or cut, and the heap counts the new bin alone.
#[test]
fn a_resize_keeps_the_shorter_prefix_and_zeroes_past_it() {
    assert_eq!(
        clean("<?php echo block_resized(24, 64), \"\\n\", block_resized(3000, 100), \"\\n\";"),
        "64 true true 64\n100 true true 112\n"
    );
}

/// A block freed by its holder and one kept to the request's end and
/// freed there both leave the armed sensor nothing to name.
#[test]
fn a_block_freed_or_kept_to_the_request_s_end_leaves_nothing() {
    assert_eq!(
        clean("<?php block_kept('free'); block_kept('request'); echo 'served';"),
        "served"
    );
}

/// A block its holder forgot is what the armed sensor names, with its
/// size and the extension whose native made it.
#[test]
fn a_forgotten_block_is_named_by_the_armed_sensor() {
    let served = served(LeakSensor::Armed, "<?php block_kept('forget');");
    let fatal = served.fatal.expect("the sensor's fatal");
    assert!(
        fatal.contains("the request ended with 1 block of its heap never freed, 24 bytes"),
        "{fatal}"
    );
    assert!(fatal.contains("allocated by storage"), "{fatal}");
}

/// Unarmed, a forgotten block is the request's end's: the heap's mass
/// release takes it, the request ends as a release build's does, and
/// the engine serves the next.
#[test]
fn unarmed_a_forgotten_block_is_released_by_the_request_s_end() {
    let engine = engine(LeakSensor::Off, None);
    for _ in 0..2 {
        let served = engine.serve_capture(
            "<?php block_kept('forget'); echo 'served';",
            Capture::default(),
        );
        served.assert_clean("served");
    }
}

/// A block or a string past the hard limit, twice the soft one, ends
/// the request where it is asked for, as php 8.5.12's `_exit(137)` ends
/// php's process: the hard limit's line on the error stream and the
/// status, nothing after it served; and the engine serves the next
/// request.
#[test]
fn a_refused_allocation_ends_the_request_in_the_limit_s_fatal() {
    let engine = engine(LeakSensor::Armed, Some("2M"));
    for src in [
        "<?php block_refused(4194304); echo 'unreached';",
        "<?php string_refused(4194304, 0); echo 'unreached';",
        "<?php string_refused(2050, 4194304); echo 'unreached';",
    ] {
        let served = engine.serve_capture(src, Capture::default());
        assert!(!served.stdout_text().contains("unreached"));
        assert_eq!(served.exit_code, 137, "{src}");
        let stderr = String::from_utf8_lossy(&served.stderr);
        assert!(
            stderr.starts_with("Hard memory limit of 4194304 bytes exhausted"),
            "{src}: {stderr}"
        );
    }
    let after = engine.serve_capture("<?php echo block_held(24);", Capture::default());
    assert_eq!(after.stdout_text(), "24 true true 0");
}

/// A fresh string grows bin by bin and page by page as `erealloc` grows
/// it, and a length set in place keeps what it holds until the string
/// is given up.
#[test]
fn a_fresh_string_grows_on_erealloc_s_schedule() {
    assert_eq!(clean("<?php echo string_grown();"), "2560 4096 8192 8192 0");
}

/// A length set in place keeps the grown allocation; a truncate moves
/// the string to the bin its length needs.
#[test]
fn a_set_length_keeps_its_allocation_and_a_truncate_does_not() {
    assert_eq!(
        clean("<?php echo string_cut(false), \"\\n\", string_cut(true);"),
        "4096 4096 true 0\n4096 32 true 0"
    );
}

/// A fresh string of no bytes or of one is a block of its own, the
/// 32-byte bin's, and never an interned string.
#[test]
fn a_short_fresh_string_is_its_own_block() {
    assert_eq!(clean("<?php echo string_short();"), "32 32");
}
