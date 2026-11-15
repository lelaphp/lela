//! The C symbols php's `test.c` exports for ext/ffi's tests
//! (`PHP_ZEND_TEST_API`, `Z/ext/zend_test/test.c:1798-1910`), each under
//! php's name and with php's C layout, so `FFI::cdef` without a library
//! resolves it through `dlsym(RTLD_DEFAULT)` in the binary that carries
//! this crate, as it resolves php's in the binary that carries php's,
//! where the platform's linker exports an executable's global symbols: macOS's
//! does, and Linux's only under `-rdynamic`, which the hosts' link does
//! not pass yet.
//! They are `zend_test`'s own C, a fixture of its tests, and not php's
//! internals, which the engine does not export
//! (`phpt:ffi-php-own-symbols`).
//!
//! The variables are C's: written and read by the C that FFI calls and by
//! FFI itself, through their addresses, and never by Rust but where a
//! function here reads one as php's C reads it. Each starts as php's
//! does, zero or null, at the process's start, and lasts as php's does,
//! for the process.
//!
//! Left out: `bug78270`, `ZEND_FASTCALL`, which only Windows' 32-bit ABI
//! makes a test of; `copy_file_range`, which php defines only where the
//! platform has the call; and the `bug_gh9090_*` set, whose functions
//! print through `php_printf`, which C with no request in hand cannot
//! reach here.

use std::ffi::{c_char, c_int, c_short, c_void};

/// `struct bug79096` (`Z/ext/zend_test/php_test.h:87`).
#[repr(C)]
pub(crate) struct Bug79096 {
    a: u64,
    b: u64,
}

/// `struct bug79096 bug79096(void)`: a struct of two 64-bit fields
/// returned by value, both one.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn bug79096() -> Bug79096 {
    Bug79096 { a: 1, b: 1 }
}

/// `void bug79532(off_t *array, size_t elems)`: each element its index.
///
/// # Safety
///
/// `array` holds `elems` writable `off_t`s, as C's caller hands it.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn bug79532(array: *mut libc::off_t, elems: usize) {
    if elems == 0 {
        return;
    }
    // SAFETY: the caller hands `elems` writable `off_t`s at `array`, and
    // nothing else reaches them while C's call runs.
    let slots = unsafe { std::slice::from_raw_parts_mut(array, elems) };
    for (slot, value) in slots.iter_mut().zip(0..) {
        *slot = value;
    }
}

/// `int *(*bug79177_cb)(void)`: the callback [`bug79177`] calls, which
/// FFI sets to a PHP closure.
#[unsafe(no_mangle)]
pub(crate) static mut bug79177_cb: Option<unsafe extern "C" fn() -> *mut c_int> = None;

/// `void bug79177(void)`: [`bug79177_cb`] called and its answer dropped.
/// php's calls it unchecked; a null one is not called here.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn bug79177() {
    // SAFETY: a read by value of a C variable that PHP code writes through
    // FFI, as php's C reads it. Like php's, which a threaded php shares
    // across its threads too, it is the process's: two requests that
    // write it while a third calls this race as they race in php, and
    // the tests that reach it run one request.
    let callback = unsafe { bug79177_cb };
    if let Some(callback) = callback {
        // SAFETY: the callback FFI set, which takes nothing.
        unsafe { callback() };
    }
}

/// `bug80847_01` (`Z/ext/zend_test/test.c:1829`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Bug80847First {
    b: u64,
    c: f64,
}

/// `bug80847_02` (`Z/ext/zend_test/test.c:1833`).
#[repr(C)]
pub(crate) struct Bug80847Second {
    a: Bug80847First,
}

/// `bug80847_02 ffi_bug80847(bug80847_02 s)`: a nested struct passed and
/// returned by value, `b` ten more and `c` ten less, `b` wrapping as C's
/// `uint64_t` does.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn ffi_bug80847(mut s: Bug80847Second) -> Bug80847Second {
    s.a.b = s.a.b.wrapping_add(10);
    s.a.c -= 10.0;
    s
}

/// `int gh11934b_ffi_var_test_cdata`: a variable FFI writes and reads.
#[unsafe(no_mangle)]
pub(crate) static mut gh11934b_ffi_var_test_cdata: c_int = 0;

/// `struct bug_gh16013_int_struct` (`Z/ext/zend_test/test.c:1877`).
#[repr(C)]
pub(crate) struct BugGh16013IntStruct {
    field: c_int,
}

/// `char bug_gh16013_return_char(void)`: `'A'`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn bug_gh16013_return_char() -> c_char {
    c_char::from_ne_bytes([b'A'])
}

/// `bool bug_gh16013_return_bool(void)`: `true`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn bug_gh16013_return_bool() -> bool {
    true
}

/// `short bug_gh16013_return_short(void)`: 12345.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn bug_gh16013_return_short() -> c_short {
    12345
}

/// `int bug_gh16013_return_int(void)`: 123456789.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn bug_gh16013_return_int() -> c_int {
    123_456_789
}

/// `enum bug_gh16013_enum` (`Z/ext/zend_test/test.c:1872`), laid out
/// as C lays an enum out, in an `int`'s room.
#[repr(C)]
pub(crate) enum BugGh16013Enum {
    #[expect(dead_code, reason = "C's case, which no function here answers")]
    A = 1,
    B = 2,
}

/// `enum bug_gh16013_enum bug_gh16013_return_enum(void)`:
/// `BUG_GH16013_B`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn bug_gh16013_return_enum() -> BugGh16013Enum {
    BugGh16013Enum::B
}

/// `struct bug_gh16013_int_struct bug_gh16013_return_struct(void)`: its
/// field 123456789.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn bug_gh16013_return_struct() -> BugGh16013IntStruct {
    BugGh16013IntStruct { field: 123_456_789 }
}

/// Every symbol above, by address: nothing in Rust calls or reads them,
/// and a linker drops what nothing reaches, so this table, which
/// `#[used]` keeps in the binary (`no_dead_strip` on Mach-O), reaches
/// each one for `dlsym` to find.
#[used]
static EXPORTED: Exported = Exported([
    bug79096 as *const c_void,
    bug79532 as *const c_void,
    bug79177 as *const c_void,
    (&raw const bug79177_cb).cast(),
    ffi_bug80847 as *const c_void,
    (&raw const gh11934b_ffi_var_test_cdata).cast(),
    bug_gh16013_return_char as *const c_void,
    bug_gh16013_return_bool as *const c_void,
    bug_gh16013_return_short as *const c_void,
    bug_gh16013_return_int as *const c_void,
    bug_gh16013_return_enum as *const c_void,
    bug_gh16013_return_struct as *const c_void,
]);

/// The addresses [`EXPORTED`] holds.
struct Exported(
    #[expect(dead_code, reason = "the linker reads the table, and no Rust does")]
    [*const c_void; 12],
);

// SAFETY: the addresses are never read through; a table of them is the
// same on every thread.
unsafe impl Sync for Exported {}
