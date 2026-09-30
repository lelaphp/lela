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
//! is neither, there is no area, as php's `clobber_error` leaves none.

use std::ffi::{CStr, c_char, c_int};
use std::sync::atomic::AtomicU8;

/// The area, `ps_buffer` for `ps_buffer_size` bytes; `None` where there
/// is none. Called once, first in `main`, before any thread starts and
/// before anything but the host's own copy of its arguments has read the
/// environment or `argv`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn save_ps_args() -> Option<&'static [AtomicU8]> {
    let (count, vector, environment) = process_args()?;
    let count = usize::try_from(count).ok().filter(|&count| count > 0)?;
    let strings = |at: *mut *mut c_char, count: usize| -> Vec<*mut c_char> {
        (0..count)
            .map(|i| {
                // SAFETY: `at` is the start of an array of at least `count`
                // pointers the C runtime laid out, `argc` for `argv`, and
                // for `environ` up to the NULL `count_of` found.
                unsafe { *at.add(i) }
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
    let size = (end as usize).checked_sub(start as usize)?;
    move_environ(&env);
    point_argv_at_copy(&args);
    for i in 1..count {
        // SAFETY: `argv` holds `argc` pointers, which the C runtime handed
        // this process to own; each after the first now points at the
        // area's last byte, a NUL, as php's `save_argv[i]` does.
        unsafe { *vector.add(i) = end };
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
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn save_ps_args() -> Option<&'static [AtomicU8]> {
    None
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

/// `*_NSGetArgv() = new_argv`: the standard library reads its arguments
/// from there on macOS, so it reads copies from here on.
#[cfg(target_os = "macos")]
fn point_argv_at_copy(args: &[*mut c_char]) {
    let copied = copies(args);
    // SAFETY: `_NSGetArgv` answers the address of the C runtime's `argv`
    // pointer, which nothing else writes: no thread has started yet.
    unsafe { *libc::_NSGetArgv() = copied };
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
extern "C" fn capture(argc: c_int, argv: *mut *mut c_char, _env: *mut *mut c_char) {
    let _ = ARGS.set((argc, argv as usize));
}

#[cfg(target_os = "linux")]
fn process_args() -> Option<(c_int, *mut *mut c_char, *mut *mut c_char)> {
    let &(argc, argv) = ARGS.get()?;
    // SAFETY: `environ` is the C library's, which nothing else writes: no
    // thread has started yet.
    let env = unsafe { environ };
    Some((argc, argv as *mut *mut c_char, env))
}

#[cfg(target_os = "linux")]
fn set_environ(moved: *mut *mut c_char) {
    // SAFETY: as above.
    unsafe { environ = moved };
}

/// On Linux the standard library keeps its own copy of the arguments'
/// pointers, which point into the area; the host has read them already.
#[cfg(target_os = "linux")]
fn point_argv_at_copy(_args: &[*mut c_char]) {}
