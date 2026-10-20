//! php's allocator hooks in its test extension,
//! `ext/zend_test/zend_mm_custom_handlers.c`, by observation (design
//! §7.1, as #1780 ruled): where php's `zend_test` installs a heap of its
//! own whose custom handlers serve each allocation from the heap beneath
//! and print it, this one puts an observer in the request's heap
//! ([`Memory::observe_allocations`]), which is told of each allocation
//! the heap still serves, and prints what php's handlers print. So what
//! `memory_get_usage()` reads while the switch is on is the request
//! heap's, where php's reads the heap it installed.
//!
//! The switch is `zend_test.zend_mm_custom_handlers.enabled`,
//! `PHP_INI_USER`: on puts the observer in where none stands, off takes
//! it out, as `OnUpdateZendTestMMCustomHandlersEnabled` calls
//! `zend_test_mm_custom_handlers_init` and `_shutdown`; `RINIT` puts it
//! in where the switch is on as the request starts, and `RSHUTDOWN`
//! takes it out.
//!
//! The lines are php's `printf`s, written to the process's standard
//! out and not through the request, as php's are written past
//! `php_output`: `Allocated %zu bytes at %p`, `Freed memory at %p`,
//! `Realloc of %zu bytes from %p to %p` and `ZendMM GC freed %zu bytes`,
//! that last with no newline. php's `Prev handlers at` line names the
//! custom handlers of the heap it found, which here are never any, so it
//! is written with five null pointers; its `Heap at %p installed` and
//! `Prev heap at %p restored` lines name the heap it installs, which an
//! observer does not have, and are not written.

use std::io::Write as _;

use php_ext::abi::{ExtensionBuilder, IniStage};
use php_ext::raising::Thrown;
use php_ext::request::{Allocation, AllocationObserverId, Ini, ObserveAllocations, Request};

use crate::{Entry, Modifiable};

/// `zend_test.zend_mm_custom_handlers.enabled`.
pub(crate) const ENABLED: &[u8] = b"zend_test.zend_mm_custom_handlers.enabled";

/// The switch's row of `zend_mm_custom_handlers.c`'s `PHP_INI_BEGIN`.
pub(crate) const ENTRY: Entry = Entry::boolean(ENABLED, b"0", Modifiable::User);

/// The switch and its `on_modify`; the extension's `RINIT` and
/// `RSHUTDOWN` serve [`rinit`] and [`rshutdown`], which put the observer
/// in and take it out at the request's edges. Turned off, the switch writes
/// nothing, as php's `zend_test_mm_custom_handlers_shutdown` returns at
/// once where no heap is installed:
///
/// ```php
/// var_dump(ini_get('zend_test.zend_mm_custom_handlers.enabled'));
/// var_dump(ini_set('zend_test.zend_mm_custom_handlers.enabled', '0'), ini_get('zend_test.zend_mm_custom_handlers.enabled'));
/// ```
/// ```expect
/// string(1) "0"
/// string(1) "0"
/// string(1) "0"
/// ```
pub(crate) fn declared(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .ini(vec![ENTRY.decl()])
        .on_modify(ENABLED, on_update_enabled)
}

/// Where the observer stands in the request's heap while the switch is
/// on, php's `ZT_G(observed_heap)`.
#[derive(Debug, Default)]
pub(crate) struct ObservedHeap(Option<AllocationObserverId>);

/// `zend_test_mm_custom_handlers_rinit`: the observer put in where the
/// switch is on as the request starts.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `rinit` slot's shape, which a request start that throws needs"
)]
pub(crate) fn rinit(req: &mut Request) -> Result<(), Thrown> {
    if req.ini().get(ENABLED).is_some_and(Ini::parse_bool) {
        install(req);
    }
    Ok(())
}

/// `zend_test_mm_custom_handlers_rshutdown`: the observer taken out.
pub(crate) fn rshutdown(req: &mut Request) {
    remove(req);
}

/// `OnUpdateZendTestMMCustomHandlersEnabled`: no value is refused; on
/// puts the observer in and off takes it out, then the entry takes the
/// value. As the engine starts there is no request heap to observe, so
/// the value is only taken, and [`rinit`] reads it.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update_enabled(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let Some(value) = value else {
        return Ok(false);
    };
    if req.ini().stage() == IniStage::Startup {
        return Ok(true);
    }
    if Ini::parse_bool(value) {
        install(req);
    } else {
        remove(req);
    }
    Ok(true)
}

/// `zend_test_mm_custom_handlers_init`: nothing where the observer
/// stands already; otherwise the line naming the handlers found, none,
/// and the observer put in.
fn install(req: &mut Request) {
    if req.state::<crate::State>().allocations.0.is_some() {
        return;
    }
    print(format_args!("Prev handlers at 0x0, 0x0, 0x0, 0x0, 0x0\n"));
    let at = req.memory_mut().observe_allocations(Box::new(Printer));
    req.state::<crate::State>().allocations.0 = Some(at);
}

/// `zend_test_mm_custom_handlers_shutdown`: the observer taken out,
/// where it stands.
fn remove(req: &mut Request) {
    if let Some(at) = req.state::<crate::State>().allocations.0.take() {
        req.memory_mut().unobserve_allocations(at);
    }
}

/// The observer: `observe_malloc`, `observe_free`, `observe_realloc` and
/// `observe_gc`'s `printf`s, each after the heap served what it prints.
struct Printer;

impl ObserveAllocations for Printer {
    fn observe(&mut self, what: Allocation) {
        match what {
            Allocation::Allocated { at, size } => {
                print(format_args!("Allocated {size} bytes at {at:#x}\n"));
            }
            Allocation::Freed { at } => print(format_args!("Freed memory at {at:#x}\n")),
            Allocation::Reallocated { from, to, size } => print(format_args!(
                "Realloc of {size} bytes from {from:#x} to {to:#x}\n"
            )),
            Allocation::Collected { freed } => {
                print(format_args!("ZendMM GC freed {freed} bytes"));
            }
        }
    }
}

/// php's `printf`: the process's standard out, past the request's.
/// A write the process cannot make is dropped, as `printf`'s is.
fn print(line: std::fmt::Arguments<'_>) {
    std::io::stdout().lock().write_fmt(line).ok();
}
