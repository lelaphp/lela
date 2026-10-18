//! libedit's readline layer, the library the pinned php links
//! (`/usr/lib/libedit.3.dylib`; design §7.2, revision 72; #1540): the
//! calls and the variables of `<editline/readline.h>` that `readline.c`
//! and `readline_cli.c` reach, declared by hand, no bindgen. The one home
//! of this crate's `unsafe` (`unsafe.homes`' `ext/*/src/ffi/`).
//!
//! The layer is one per process: its line, its history, its completion
//! function and its callback handler are the library's own variables, and so
//! is the one thing this binding keeps beside them, the [`Lent`] record
//! of the call in progress. A native that calls into the library lends
//! its request for that call ([`lend`]); a callback the library makes
//! from inside it finds the request there and nowhere else, as php's
//! callbacks find theirs in `EG(current_execute_data)`. Outside a lending
//! call a callback finds none and answers as php does with nothing to
//! call.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr;

use php_ext::request::Request;

mod sys {
    use std::ffi::{c_char, c_int};

    pub(super) type Generator = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_char;
    pub(super) type Attempted =
        unsafe extern "C" fn(*const c_char, c_int, c_int) -> *mut *mut c_char;
    pub(super) type LineHandler = unsafe extern "C" fn(*mut c_char);

    #[link(name = "edit")]
    unsafe extern "C" {
        pub(super) static rl_library_version: *const c_char;
        pub(super) static mut rl_readline_name: *mut c_char;
        pub(super) static mut rl_line_buffer: *mut c_char;
        pub(super) static mut rl_point: c_int;
        pub(super) static mut rl_end: c_int;
        pub(super) static mut rl_attempted_completion_function: Option<Attempted>;
        pub(super) static mut rl_attempted_completion_over: c_int;
        pub(super) static mut rl_special_prefixes: *mut c_char;
        pub(super) static mut rl_completion_append_character: c_int;
        pub(super) fn readline(prompt: *const c_char) -> *mut c_char;
        pub(super) fn using_history();
        pub(super) fn add_history(line: *const c_char) -> c_int;
        pub(super) fn clear_history();
        pub(super) fn read_history(file: *const c_char) -> c_int;
        pub(super) fn write_history(file: *const c_char) -> c_int;
        pub(super) fn rl_completion_matches(
            text: *const c_char,
            generator: Generator,
        ) -> *mut *mut c_char;
        pub(super) fn rl_callback_handler_install(prompt: *const c_char, handler: LineHandler);
        pub(super) fn rl_callback_read_char();
        pub(super) fn rl_callback_handler_remove();
        pub(super) fn rl_redisplay();
        pub(super) fn rl_on_new_line() -> c_int;
    }
}

/// The call in progress that lent its request: the request, and the
/// candidates a completion is serving to `rl_completion_matches`, the
/// array php's generator walks.
struct Lent {
    req: *mut Request,
    candidates: Vec<CString>,
    at: usize,
}

/// `pthread_once` for [`KEY`], made once for the process.
static mut ONCE: libc::pthread_once_t = libc::PTHREAD_ONCE_INIT;

/// The key of the per-thread slot that holds the record of the lending
/// call in progress on the thread, null outside one: libedit's callbacks
/// carry no user data, so what php keeps per thread under `ZEND_TLS` is
/// kept in a slot of a `pthread_key_t`, made once through `pthread_once`
/// (design §7.2's readline row, revision 76; #1606, #2821). The key is
/// written once, by [`key`]'s `make_key`, and read after; no request's
/// pointer stands in a process-wide static.
static mut KEY: libc::pthread_key_t = 0;

/// `pthread_once`'s routine parameter as `libc` declares it for the
/// target: Apple's `<pthread.h>` takes a nullable `void (*)(void)`,
/// which `libc` spells `Option<unsafe extern "C" fn()>`, and glibc's a
/// bare `extern "C" fn()`.
#[cfg(target_vendor = "apple")]
type Routine = Option<unsafe extern "C" fn()>;
#[cfg(not(target_vendor = "apple"))]
type Routine = extern "C" fn();

/// The slot's key, made on the first ask.
fn key() -> libc::pthread_key_t {
    /// `pthread_once`'s routine: the slot's key made. Named only here,
    /// so its one caller is the `pthread_once` below, which serialises
    /// it and runs it once for the process.
    extern "C" fn make_key() {
        let key = &raw mut KEY;
        // SAFETY: called once, by `pthread_once`, before any read of
        // `KEY`; the slot needs no destructor, since a record lives on
        // its lending frame and the slot is null outside one.
        unsafe {
            libc::pthread_key_create(key, None);
        }
    }
    #[cfg(target_vendor = "apple")]
    let routine: Routine = Some(make_key);
    #[cfg(not(target_vendor = "apple"))]
    let routine: Routine = make_key;
    let once = &raw mut ONCE;
    // SAFETY: `ONCE` is only ever handed to `pthread_once`, which
    // serialises the one call of `make_key`.
    unsafe {
        libc::pthread_once(once, routine);
    }
    // SAFETY: written by `make_key`, which `pthread_once` ran to its end
    // before returning here, and never written again.
    unsafe { KEY }
}

/// The thread's slot: the record of the lending call in progress, null
/// outside one.
fn slot() -> *mut Lent {
    let key = key();
    // SAFETY: a key `pthread_key_create` made; the slot holds what
    // `set_slot` put there on this thread, or null.
    unsafe { libc::pthread_getspecific(key) }.cast()
}

/// The thread's slot set to `record`.
fn set_slot(record: *mut Lent) {
    let key = key();
    // SAFETY: a key `pthread_key_create` made, and the value one this
    // thread's lending frame owns, or null.
    unsafe {
        libc::pthread_setspecific(key, record.cast());
    }
}

/// `call` with `req` lent to whatever the library calls back while it
/// runs, the lending before it put back after, so a callback that lends
/// again nests.
fn lend<R>(req: &mut Request, call: impl FnOnce() -> R) -> R {
    let mut lent = Lent {
        req,
        candidates: Vec::new(),
        at: 0,
    };
    lending(&mut lent, call)
}

/// `call` with `record` in the thread's slot, the record before it put
/// back after.
fn lending<R>(record: &mut Lent, call: impl FnOnce() -> R) -> R {
    let outer = slot();
    set_slot(record);
    let answer = call();
    set_slot(outer);
    answer
}

/// The lending call's record, `None` outside one.
fn lent<'a>() -> Option<&'a mut Lent> {
    let at = slot();
    // SAFETY: set by `lending` to a record on its frame, live until
    // `lending` returns, which it does not while the library calls back;
    // the lending native touches neither the record nor its request until
    // then, so this is the one live borrow of both.
    unsafe { at.as_mut() }
}

/// The lending call's request.
fn request<'a>(lent: &Lent) -> &'a mut Request {
    // SAFETY: as in `lent`; the request was a `&mut` when it was lent.
    unsafe { &mut *lent.req }
}

/// `s` as a C string, cut at its first NUL as C reads it.
fn c(s: &[u8]) -> CString {
    CString::new(php_ext::coerce::c_str(s)).unwrap_or_default()
}

/// The bytes of a C string, empty for null (`SAFE_STRING`).
///
/// # Safety
/// `s` is null or a NUL-terminated string live for the call.
unsafe fn bytes(s: *const c_char) -> Vec<u8> {
    if s.is_null() {
        return Vec::new();
    }
    // SAFETY: the caller's contract.
    unsafe { CStr::from_ptr(s) }.to_bytes().to_vec()
}

/// `strdup` of `s`, which the library may `free`.
fn dup(s: &[u8]) -> *mut c_char {
    let s = c(s);
    // SAFETY: a NUL-terminated string live for the call.
    unsafe { libc::strdup(s.as_ptr()) }
}

/// `readline(prompt)`: the line read, without its end, or `None` at the
/// end of input; the request lent to the completion function.
pub(crate) fn readline(req: &mut Request, prompt: Option<&[u8]>) -> Option<Vec<u8>> {
    let prompt = prompt.map(c);
    let at = prompt.as_ref().map_or(ptr::null(), |p| p.as_ptr());
    // SAFETY: a NUL-terminated prompt or null, live for the call.
    let line = lend(req, || unsafe { sys::readline(at) });
    if line.is_null() {
        return None;
    }
    // SAFETY: the line the library allocated, NUL-terminated.
    let read = unsafe { bytes(line) };
    // SAFETY: allocated with `malloc` and handed to the caller.
    unsafe { libc::free(line.cast()) };
    Some(read)
}

/// `add_history(line)`.
pub(crate) fn add_history(line: &[u8]) {
    let line = c(line);
    // SAFETY: a NUL-terminated string the library copies.
    unsafe { sys::add_history(line.as_ptr()) };
}

/// `using_history()` then `clear_history()`, which libedit needs
/// initialised first.
pub(crate) fn clear_history() {
    // SAFETY: no argument.
    unsafe { sys::using_history() };
    // SAFETY: no argument.
    unsafe { sys::clear_history() };
}

/// `read_history(file)`, the library's `~/.history` for `None`; whether
/// it read.
pub(crate) fn read_history(file: Option<&[u8]>) -> bool {
    let file = file.map(c);
    let at = file.as_ref().map_or(ptr::null(), |f| f.as_ptr());
    // SAFETY: a NUL-terminated path or null, live for the call.
    unsafe { sys::read_history(at) == 0 }
}

/// `write_history(file)`; whether it wrote.
pub(crate) fn write_history(file: Option<&[u8]>) -> bool {
    let file = file.map(c);
    let at = file.as_ref().map_or(ptr::null(), |f| f.as_ptr());
    // SAFETY: as in `read_history`.
    unsafe { sys::write_history(at) == 0 }
}

/// The library's `rl_library_version`.
pub(crate) fn library_version() -> Vec<u8> {
    // SAFETY: the library's static string, never written.
    let at = unsafe { sys::rl_library_version };
    // SAFETY: as above.
    unsafe { bytes(at) }
}

/// `rl_line_buffer`.
pub(crate) fn line_buffer() -> Vec<u8> {
    // SAFETY: the library's global, read by copy.
    let at = unsafe { sys::rl_line_buffer };
    // SAFETY: the library's line or one this binding gave it.
    unsafe { bytes(at) }
}

/// The copy of a value php last gave the library, freed when the next
/// one replaces it (`php_readline_custom_line_buffer`,
/// `php_readline_custom_readline_name`).
static mut OWNED_LINE: *mut c_char = ptr::null_mut();
static mut OWNED_NAME: *mut c_char = ptr::null_mut();

/// `rl_line_buffer` and `rl_end` set to a copy of `value`, as libedit's
/// branch of `readline_info()` sets them.
pub(crate) fn set_line_buffer(value: &[u8]) {
    let copy = dup(value);
    // SAFETY: the library's global, written with a copy it may read.
    unsafe { sys::rl_line_buffer = copy };
    // SAFETY: the copy this binding gave before, or null.
    let old = unsafe { OWNED_LINE };
    // SAFETY: allocated by `strdup` and no longer the library's.
    unsafe { libc::free(old.cast()) };
    // SAFETY: this binding's own global.
    unsafe { OWNED_LINE = copy };
    let end = c_int::try_from(c(value).as_bytes().len()).unwrap_or(c_int::MAX);
    // SAFETY: the library's global.
    unsafe { sys::rl_end = end };
}

/// `rl_point`.
pub(crate) fn point() -> i64 {
    // SAFETY: the library's global, read by copy.
    i64::from(unsafe { sys::rl_point })
}

/// `rl_end`.
pub(crate) fn end() -> i64 {
    // SAFETY: the library's global, read by copy.
    i64::from(unsafe { sys::rl_end })
}

/// `rl_readline_name`.
pub(crate) fn readline_name() -> Vec<u8> {
    // SAFETY: the library's global, read by copy.
    let at = unsafe { sys::rl_readline_name };
    // SAFETY: the library's name or one this binding gave it.
    unsafe { bytes(at) }
}

/// `rl_readline_name` set to a copy of `value`.
pub(crate) fn set_readline_name(value: &[u8]) {
    let copy = dup(value);
    // SAFETY: the library's global, written with a copy it may read.
    unsafe { sys::rl_readline_name = copy };
    // SAFETY: the copy this binding gave before, or null.
    let old = unsafe { OWNED_NAME };
    // SAFETY: allocated by `strdup` and no longer the library's.
    unsafe { libc::free(old.cast()) };
    // SAFETY: this binding's own global.
    unsafe { OWNED_NAME = copy };
}

/// `rl_attempted_completion_over`.
pub(crate) fn attempted_completion_over() -> i64 {
    // SAFETY: the library's global, read by copy.
    i64::from(unsafe { sys::rl_attempted_completion_over })
}

/// `rl_attempted_completion_over = value`, an `int` as C stores a
/// `zend_long` into one.
pub(crate) fn set_attempted_completion_over(value: i64) {
    #[allow(clippy::cast_possible_truncation)]
    let value = value as c_int;
    // SAFETY: the library's global.
    unsafe { sys::rl_attempted_completion_over = value };
}

/// `rl_completion_append_character = c`.
pub(crate) fn set_completion_append_character(c: u8) {
    // SAFETY: the library's global.
    unsafe { sys::rl_completion_append_character = c_int::from(c) };
}

/// Which completion function the library calls.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Completion {
    /// `php_readline_completion_cb`, the program's callback.
    User,
    /// `cli_code_completion`, the shell's own.
    Shell,
}

/// `rl_attempted_completion_function` set to `completion`'s.
pub(crate) fn complete_with(completion: Completion) {
    let function: sys::Attempted = match completion {
        Completion::User => user_completion,
        Completion::Shell => shell_completion,
    };
    // SAFETY: the library's global, a function that lives as long.
    unsafe { sys::rl_attempted_completion_function = Some(function) };
}

/// Whether the program's callback is the completion function.
pub(crate) fn completes_with_user() -> bool {
    // SAFETY: the library's global, read by copy.
    let current = unsafe { sys::rl_attempted_completion_function };
    let user: sys::Attempted = user_completion;
    current.is_some_and(|f| ptr::fn_addr_eq(f, user))
}

/// `rl_special_prefixes = "$"`, the shell's.
pub(crate) fn special_prefixes_dollar() {
    // SAFETY: the library's global, a static string it only reads.
    unsafe { sys::rl_special_prefixes = c"$".as_ptr().cast_mut() };
}

/// What a completion answered: nothing, the empty match list libedit
/// reads three slots of, or the candidates `rl_completion_matches`
/// walks with `text`.
pub(crate) enum Matches {
    None,
    Empty,
    Candidates(Vec<Vec<u8>>),
}

/// `php_readline_completion_cb`: [`crate::user_completion`] over the
/// lent request.
unsafe extern "C" fn user_completion(
    text: *const c_char,
    start: c_int,
    end: c_int,
) -> *mut *mut c_char {
    // SAFETY: the word the library hands its completion function.
    let word = unsafe { bytes(text) };
    let Some(lent) = lent() else {
        return ptr::null_mut();
    };
    let answer = crate::user_completion(request(lent), &word, start.into(), end.into());
    // SAFETY: the word, as above.
    unsafe { matches(lent, text, answer) }
}

/// `cli_code_completion`: [`crate::shell::completion`] over the lent
/// request.
unsafe extern "C" fn shell_completion(
    text: *const c_char,
    _start: c_int,
    _end: c_int,
) -> *mut *mut c_char {
    // SAFETY: the word the library hands its completion function.
    let word = unsafe { bytes(text) };
    let Some(lent) = lent() else {
        return ptr::null_mut();
    };
    let answer = crate::shell::completion(request(lent), &word);
    // SAFETY: the word, as above.
    unsafe { matches(lent, text, answer) }
}

/// The list the library is handed for `answer`.
///
/// # Safety
/// `text` is the word the library handed the completion function.
unsafe fn matches(lent: &mut Lent, text: *const c_char, answer: Matches) -> *mut *mut c_char {
    match answer {
        Matches::None => ptr::null_mut(),
        Matches::Empty => {
            // SAFETY: three null slots; libedit reads `matches[2]`.
            let list: *mut *mut c_char =
                unsafe { libc::calloc(3, size_of::<*mut c_char>()) }.cast();
            if !list.is_null() {
                // SAFETY: the first of the three slots just allocated.
                unsafe { *list = dup(b"") };
            }
            list
        }
        Matches::Candidates(candidates) => {
            lent.candidates = candidates.iter().map(|s| c(s)).collect();
            lent.at = 0;
            // SAFETY: the word, and a generator that reads the record
            // this call holds.
            unsafe { sys::rl_completion_matches(text, next_candidate) }
        }
    }
}

/// `_readline_command_generator` and `cli_completion_generator` over the
/// candidates the completion kept: the next one, a copy the library
/// frees, or null when they are spent.
unsafe extern "C" fn next_candidate(_text: *const c_char, _state: c_int) -> *mut c_char {
    let Some(lent) = lent() else {
        return ptr::null_mut();
    };
    let Some(candidate) = lent.candidates.get(lent.at) else {
        return ptr::null_mut();
    };
    lent.at += 1;
    dup(candidate.as_bytes())
}

/// `rl_callback_handler_install(prompt, php_rl_callback_handler)`.
pub(crate) fn callback_handler_install(prompt: &[u8]) {
    let prompt = c(prompt);
    // SAFETY: a NUL-terminated prompt the library copies, and a handler
    // that lives as long as it.
    unsafe { sys::rl_callback_handler_install(prompt.as_ptr(), line_handler) };
}

/// `rl_callback_read_char()`, the request lent to the handler.
pub(crate) fn callback_read_char(req: &mut Request) {
    // SAFETY: no argument.
    lend(req, || unsafe { sys::rl_callback_read_char() });
}

/// `rl_callback_handler_remove()`.
pub(crate) fn callback_handler_remove() {
    // SAFETY: no argument.
    unsafe { sys::rl_callback_handler_remove() };
}

/// `php_rl_callback_handler`: [`crate::callback_line`] over the lent
/// request, with the line, `None` at the end of input.
unsafe extern "C" fn line_handler(line: *mut c_char) {
    let read = (!line.is_null()).then(|| {
        // SAFETY: the line the library hands its handler.
        unsafe { bytes(line) }
    });
    if let Some(lent) = lent() {
        crate::callback_line(request(lent), read);
    }
}

/// `using_history()` then `rl_redisplay()`, as libedit's branch has it
/// (bug #72538).
pub(crate) fn redisplay() {
    // SAFETY: no argument.
    unsafe { sys::using_history() };
    // SAFETY: no argument.
    unsafe { sys::rl_redisplay() };
}

/// `rl_on_new_line()`.
pub(crate) fn on_new_line() {
    // SAFETY: no argument.
    unsafe { sys::rl_on_new_line() };
}

#[cfg(test)]
mod slot_tests {
    use super::{Lent, lending, slot};

    /// A record with no request, enough for the slot, which holds the
    /// record and reads none of it.
    fn record() -> Lent {
        Lent {
            req: std::ptr::null_mut(),
            candidates: Vec::new(),
            at: 0,
        }
    }

    /// A lending inside another's call puts its own record in the slot and
    /// the outer one back as it returns, and the slot is empty outside both.
    #[test]
    fn a_lending_nests_and_puts_the_outer_record_back() {
        let (mut outer, mut inner) = (record(), record());
        let (outer_at, inner_at): (*mut Lent, *mut Lent) = (&raw mut outer, &raw mut inner);
        assert!(slot().is_null());
        lending(&mut outer, || {
            assert_eq!(slot(), outer_at);
            lending(&mut inner, || assert_eq!(slot(), inner_at));
            assert_eq!(slot(), outer_at);
        });
        assert!(slot().is_null());
    }

    /// Each thread's slot is its own: a thread started inside another's
    /// lending finds its slot empty, lends its own record, and leaves the
    /// first thread's record where it stood.
    #[test]
    #[allow(clippy::disallowed_methods)] // a test's own thread, which no fork waits on
    fn two_threads_lend_records_of_their_own() {
        let mut first = record();
        let first_at: *mut Lent = &raw mut first;
        lending(&mut first, || {
            let joined = std::thread::spawn(|| {
                assert!(slot().is_null());
                let mut second = record();
                let second_at: *mut Lent = &raw mut second;
                lending(&mut second, || assert_eq!(slot(), second_at));
                assert!(slot().is_null());
            })
            .join();
            assert!(joined.is_ok());
            assert_eq!(slot(), first_at);
        });
    }
}
