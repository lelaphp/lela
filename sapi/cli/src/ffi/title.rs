//! `save_ps_args` (`Z/sapi/cli/ps_title.c:147`) under
//! `PS_USE_CLOBBER_ARGV`, which php builds on Linux and macOS: the
//! process's `argv` strings, contiguous, and the environment strings that
//! follow them become the area `ps` shows the command line from; the
//! environment is moved out of it first, every string copied and
//! `environ` pointed at the copies, and on macOS `*_NSGetArgv()` at a
//! copy of `argv`, which is what the standard library reads its
//! arguments from there; `argv[1..]` are pointed at the NUL that ends the
//! area. The host reads its arguments before this, so what it parses is
//! never the title. Where the strings are not contiguous, or the system
//! is neither, there is no area, as php's `clobber_error` leaves none. On
//! Linux the area is found through glibc alone, which hands every
//! `.init_array` function `argc`, `argv` and `envp`, as the standard
//! library's own hook is gated (`library/std/src/sys/args/unix.rs`);
//! musl calls them with nothing, so a Linux C library other than glibc
//! leaves no area.

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
use std::ffi::{CStr, c_char, c_int};
use std::sync::atomic::AtomicU8;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
use std::sync::atomic::Ordering;

/// The area, `ps_buffer` for `ps_buffer_size` bytes; `None` where there
/// is none. Called once, first in `main`, before any thread starts and
/// before anything but the host's own copy of its arguments has read the
/// environment or `argv`.
#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn save_ps_args() -> Option<&'static [AtomicU8]> {
    let (count, vector, environment) = process_args()?;
    let count = usize::try_from(count).ok().filter(|&count| count > 0)?;
    let strings = |at: *mut *mut c_char, count: usize| -> Vec<*mut c_char> {
        (0..count)
            .map(|i| {
                // SAFETY: `at` is the start of an array of at least `count`
                // pointers the C runtime laid out, `argc` for `argv`, and
                // for `environ` up to the NULL `count_of` found.
                let slot = unsafe { at.add(i) };
                // SAFETY: the slot is inside that array, which the C runtime
                // initialised.
                unsafe { *slot }
            })
            .collect()
    };
    let args = strings(vector, count);
    let env = strings(environment, count_of(environment));
    let start = *args.first()?;
    let mut end = start;
    for (i, &arg) in args.iter().enumerate() {
        if i != 0 && end.wrapping_add(1) != arg {
            return None;
        }
        end = arg.wrapping_add(length(arg));
    }
    for &var in &env {
        if end.wrapping_add(1) == var {
            end = var.wrapping_add(length(var));
        }
    }
    let size = end.addr().checked_sub(start.addr())?;
    move_environ(&env);
    point_argv_at_copy(&args);
    for i in 1..count {
        // SAFETY: `argv` holds `argc` pointers, which the C runtime handed
        // this process to own; each after the first now points at the
        // area's last byte, a NUL, as php's `save_argv[i]` does.
        let slot = unsafe { vector.add(i) };
        // SAFETY: as above, the slot is inside the array.
        unsafe { *slot = end };
    }
    // SAFETY: the `size` bytes from `start` are the argv and environment
    // strings, contiguous as checked above, live for the whole process;
    // nothing reads them as strings from here on (the environment and
    // the arguments were copied out above), so they are this area's
    // alone, read and written only as atomic bytes.
    let area = unsafe { std::slice::from_raw_parts(start.cast::<AtomicU8>(), size) };
    Some(area)
}

/// No area on a system php builds `PS_USE_CLOBBER_ARGV` for not.
#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
pub(crate) fn save_ps_args() -> Option<&'static [AtomicU8]> {
    None
}

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
/// `strlen`.
fn length(string: *const c_char) -> usize {
    // SAFETY: every pointer this reads is an argv or environment string,
    // which the C runtime ends with a NUL.
    unsafe { CStr::from_ptr(string) }.count_bytes()
}

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
/// How many pointers stand in a NULL-ended array.
fn count_of(array: *mut *mut c_char) -> usize {
    let mut count = 0;
    loop {
        // SAFETY: the array is `environ`, ended by a NULL pointer, and the
        // loop stops at it.
        let slot = unsafe { array.add(count) };
        // SAFETY: as above, the slot is at or before the NULL.
        let at = unsafe { *slot };
        if at.is_null() {
            return count;
        }
        count += 1;
    }
}

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
/// A NULL-ended array of copies of `strings`, leaked: the process holds
/// them for the rest of its life, as php's `new_environ` and `new_argv`.
fn copies(strings: &[*mut c_char]) -> *mut *mut c_char {
    let mut copied: Vec<*mut c_char> = strings
        .iter()
        .map(|&string| {
            // SAFETY: each is an argv or environment string ended by a NUL.
            let text = unsafe { CStr::from_ptr(string) };
            text.to_owned().into_raw()
        })
        .collect();
    copied.push(std::ptr::null_mut());
    Box::leak(copied.into_boxed_slice()).as_mut_ptr()
}

#[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
/// `environ = new_environ`: the environment's strings copied out of the
/// area before it is written over.
fn move_environ(env: &[*mut c_char]) {
    let moved = copies(env);
    set_environ(moved);
}

#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)] // Linux's may find nothing captured
fn process_args() -> Option<(c_int, *mut *mut c_char, *mut *mut c_char)> {
    // SAFETY: the three answer the C runtime's own pointers to the
    // process's `argc`, `argv` and `environ`, always valid in an
    // executable.
    let at = unsafe { libc::_NSGetArgc() };
    // SAFETY: the address is the C runtime's own, valid and aligned.
    let count = unsafe { *at };
    // SAFETY: as above.
    let at = unsafe { libc::_NSGetArgv() };
    // SAFETY: the address is the C runtime's own, valid and aligned.
    let vector = unsafe { *at };
    // SAFETY: as above.
    let at = unsafe { libc::_NSGetEnviron() };
    // SAFETY: the address is the C runtime's own, valid and aligned.
    let environment = unsafe { *at };
    Some((count, vector, environment))
}

#[cfg(target_os = "macos")]
fn set_environ(moved: *mut *mut c_char) {
    // SAFETY: `_NSGetEnviron` answers the address of `environ`, which
    // nothing else writes: no thread has started yet.
    let at = unsafe { libc::_NSGetEnviron() };
    // SAFETY: as above: the C runtime's own `environ`, aligned, which no
    // other thread writes, since none has started.
    unsafe { *at = moved };
}

/// `*_NSGetArgv() = new_argv`: the standard library reads its arguments
/// from there on macOS, so it reads copies from here on.
#[cfg(target_os = "macos")]
fn point_argv_at_copy(args: &[*mut c_char]) {
    let copied = copies(args);
    // SAFETY: `_NSGetArgv` answers the address of the C runtime's `argv`
    // pointer, which nothing else writes: no thread has started yet.
    let at = unsafe { libc::_NSGetArgv() };
    // SAFETY: as above: the C runtime's own `argv` pointer, aligned,
    // which no other thread writes, since none has started.
    unsafe { *at = copied };
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
unsafe extern "C" {
    static mut environ: *mut *mut c_char;
}

/// `argc` and `argv` as glibc hands them to each `.init_array` function
/// before `main`, kept here, as the standard library keeps its own. `argv`
/// is kept as the pointer itself, so what `process_args` reads back carries
/// the provenance glibc's pointer did; an atomic is what makes a pointer a
/// `Sync` static without an `unsafe impl`. Both are written once, by
/// `capture`, and read once, by `process_args`, on the main thread before
/// any other starts, so `Relaxed` orders nothing that program order does
/// not; a null `ARGV` is a process glibc never called `capture` in.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
static ARGC: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// `argv`, as [`ARGC`] says.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
static ARGV: std::sync::atomic::AtomicPtr<*mut c_char> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[used]
#[unsafe(link_section = ".init_array")]
static CAPTURE: extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char) = capture;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
extern "C" fn capture(count: c_int, vector: *mut *mut c_char, _environment: *mut *mut c_char) {
    ARGC.store(count, Ordering::Relaxed);
    ARGV.store(vector, Ordering::Relaxed);
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn process_args() -> Option<(c_int, *mut *mut c_char, *mut *mut c_char)> {
    let vector = ARGV.load(Ordering::Relaxed);
    if vector.is_null() {
        return None;
    }
    let count = ARGC.load(Ordering::Relaxed);
    // SAFETY: `environ` is the C library's, which nothing else writes: no
    // thread has started yet.
    let environment = unsafe { environ };
    Some((count, vector, environment))
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn set_environ(moved: *mut *mut c_char) {
    // SAFETY: as above.
    unsafe { environ = moved };
}

/// On Linux the standard library keeps its own copy of the arguments'
/// pointers, which point into the area; the host has read them already.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn point_argv_at_copy(_args: &[*mut c_char]) {}
