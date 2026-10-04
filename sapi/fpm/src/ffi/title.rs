//! `fpm_env_init_main` and `fpm_env_setproctitle` on Linux and macOS
//! (`Z/sapi/fpm/fpm/fpm_env.c`), where php has no `setproctitle(3)`: the
//! process's `argv` strings and the environment strings contiguous after
//! them are the area `ps` shows, the environment copied out of it first
//! and `environ` pointed at the copies; a title is `php-fpm: ` and the
//! text over `argv[0]`, the rest of the area zeroed, and `argv[1]` NULL.

use std::ffi::{CStr, c_char, c_int};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// `SETPROCTITLE_PREFIX`.
const PREFIX: &[u8] = b"php-fpm: ";

/// The area's start and length, `fpm_env_argv[0]` and
/// `fpm_env_argv_len`, and `argv`, whose second slot a title clears.
static AREA: AtomicUsize = AtomicUsize::new(0);
static LEN: AtomicUsize = AtomicUsize::new(0);
static ARGV: AtomicUsize = AtomicUsize::new(0);

/// `fpm_env_init_main`'s half before the title: the area found and the
/// environment moved out of it. Called once, first in `main`, before any
/// thread starts and after the host has read its own arguments.
pub(crate) fn save_title_area() {
    let Some((count, vector, environment)) = process_args() else {
        return;
    };
    let Ok(count) = usize::try_from(count) else {
        return;
    };
    let strings = |at: *mut *mut c_char, count: usize| -> Vec<*mut c_char> {
        (0..count)
            .map(|i| {
                // SAFETY: `at` is an array of at least `count` pointers the
                // C runtime laid out, `argc` for `argv`, and for `environ`
                // up to the NULL `count_of` found.
                unsafe { *at.add(i) }
            })
            .collect()
    };
    let args = strings(vector, count);
    let env = strings(environment, count_of(environment));
    let (mut first, mut last): (Option<*mut c_char>, Option<*mut c_char>) = (None, None);
    for &string in args.iter().chain(&env) {
        first = first.or(Some(string));
        if last.is_none_or(|last| string == last.wrapping_add(1)) {
            last = Some(string.wrapping_add(length(string)));
        }
    }
    let (Some(first), Some(last)) = (first, last) else {
        return;
    };
    let copied = copies(&env);
    set_environ(copied);
    AREA.store(first as usize, Ordering::Relaxed);
    LEN.store(
        (last as usize).saturating_sub(first as usize),
        Ordering::Relaxed,
    );
    ARGV.store(vector as usize, Ordering::Relaxed);
}

/// `fpm_env_setproctitle`: `php-fpm: ` and `title` over the area, which
/// must be longer than the prefix and three bytes more.
pub(crate) fn set_title(title: &[u8]) {
    let len = LEN.load(Ordering::Relaxed);
    let at = AREA.load(Ordering::Relaxed);
    if at == 0 || len <= PREFIX.len() + 3 {
        return;
    }
    // SAFETY: the `len` bytes from `at` are the argv and environment
    // strings, contiguous as `save_title_area` checked, live for the whole
    // process, and read by nothing else once the environment was copied
    // out, so they are the title's alone, written only as atomic bytes.
    let area = unsafe { std::slice::from_raw_parts(at as *const AtomicU8, len) };
    let text: Vec<u8> = PREFIX
        .iter()
        .take(len - 2)
        .chain(title.iter().take(len - PREFIX.len() - 2))
        .copied()
        .collect();
    for (i, cell) in area.iter().enumerate() {
        cell.store(text.get(i).copied().unwrap_or(0), Ordering::Relaxed);
    }
    let argv = ARGV.load(Ordering::Relaxed) as *mut *mut c_char;
    // SAFETY: `argv` holds at least one pointer after `argv[0]`, the NULL
    // ending it where `argc` is 1; setting it NULL is php's own write.
    unsafe { *argv.add(1) = std::ptr::null_mut() };
}

/// `strlen`.
fn length(string: *const c_char) -> usize {
    // SAFETY: every pointer this reads is an argv or environment string,
    // which the C runtime ends with a NUL.
    unsafe { CStr::from_ptr(string) }.count_bytes()
}

/// How many pointers stand in a NULL-ended array.
fn count_of(array: *mut *mut c_char) -> usize {
    let mut count = 0;
    loop {
        // SAFETY: the array is `environ`, ended by a NULL pointer, and the
        // loop stops at it.
        let at = unsafe { *array.add(count) };
        if at.is_null() {
            return count;
        }
        count += 1;
    }
}

/// A NULL-ended array of copies of `strings`, leaked: the process holds
/// them for the rest of its life, as php's `new_environ`.
fn copies(strings: &[*mut c_char]) -> *mut *mut c_char {
    let mut copied: Vec<*mut c_char> = strings
        .iter()
        .map(|&string| {
            // SAFETY: each is an environment string ended by a NUL.
            let text = unsafe { CStr::from_ptr(string) };
            text.to_owned().into_raw()
        })
        .collect();
    copied.push(std::ptr::null_mut());
    Box::leak(copied.into_boxed_slice()).as_mut_ptr()
}

#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)] // Linux's may find nothing captured
fn process_args() -> Option<(c_int, *mut *mut c_char, *mut *mut c_char)> {
    // SAFETY: the three answer the C runtime's own pointers to the
    // process's `argc`, `argv` and `environ`, always valid in an
    // executable.
    let count = unsafe { *libc::_NSGetArgc() };
    // SAFETY: as above.
    let vector = unsafe { *libc::_NSGetArgv() };
    // SAFETY: as above.
    let environment = unsafe { *libc::_NSGetEnviron() };
    Some((count, vector, environment))
}

#[cfg(target_os = "macos")]
fn set_environ(moved: *mut *mut c_char) {
    // SAFETY: `_NSGetEnviron` answers the address of `environ`, which
    // nothing else writes: no thread has started yet.
    unsafe { *libc::_NSGetEnviron() = moved };
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    static mut environ: *mut *mut c_char;
}

/// `argc` and `argv` as glibc hands them to each `.init_array` function
/// before `main`, kept here, as the standard library keeps its own.
#[cfg(target_os = "linux")]
static ARGS: std::sync::OnceLock<(c_int, usize)> = std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array")]
static CAPTURE: extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char) = capture;

#[cfg(target_os = "linux")]
extern "C" fn capture(count: c_int, vector: *mut *mut c_char, _environment: *mut *mut c_char) {
    let _ = ARGS.set((count, vector as usize));
}

#[cfg(target_os = "linux")]
fn process_args() -> Option<(c_int, *mut *mut c_char, *mut *mut c_char)> {
    let &(count, vector) = ARGS.get()?;
    // SAFETY: `environ` is the C library's, which nothing else writes: no
    // thread has started yet.
    let environment = unsafe { environ };
    Some((count, vector as *mut *mut c_char, environment))
}

#[cfg(target_os = "linux")]
fn set_environ(moved: *mut *mut c_char) {
    // SAFETY: as above.
    unsafe { environ = moved };
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_args() -> Option<(c_int, *mut *mut c_char, *mut *mut c_char)> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn set_environ(_moved: *mut *mut c_char) {}
