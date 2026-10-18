//! The binding to libintl (design §7.2, revision 72; `unsafe.homes`'
//! `ext/*/src/ffi/`): the keg's GNU gettext, the library the oracle
//! links, which `build.rs` finds, and the one home of this crate's
//! `unsafe`. The declarations are `libintl.h`'s, written here, no
//! bindgen. On macOS `libintl.h` redirects each name to its
//! `libintl_`-prefixed entry, and php's calls land there; elsewhere the
//! C library's own names are the ones a build against it calls.
//!
//! Every function here is libintl's own lookup under the C library's
//! locale state: the thread's `uselocale(3)` locale, which ext/standard's
//! `setlocale()` installs for the request, for the category asked.
//! libintl's domain table, what `textdomain` and `bindtextdomain` set, is
//! the process's, as it is under php's ZTS build.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char, c_int, c_ulong};

/// `LC_ALL`, the category `dcgettext` refuses.
pub(crate) const LC_ALL: c_int = libc::LC_ALL;
/// `LC_MESSAGES`, the category `gettext`, `dgettext` and their plurals
/// read.
pub(crate) const LC_MESSAGES: c_int = libc::LC_MESSAGES;

/// The environment variable that names `category`'s locale, and the
/// mask `newlocale(3)` sets it under; none for a category the host does
/// not know.
fn category(category: c_int) -> Option<(&'static [u8], c_int)> {
    Some(match category {
        libc::LC_COLLATE => (b"LC_COLLATE", libc::LC_COLLATE_MASK),
        libc::LC_CTYPE => (b"LC_CTYPE", libc::LC_CTYPE_MASK),
        libc::LC_MONETARY => (b"LC_MONETARY", libc::LC_MONETARY_MASK),
        libc::LC_NUMERIC => (b"LC_NUMERIC", libc::LC_NUMERIC_MASK),
        libc::LC_TIME => (b"LC_TIME", libc::LC_TIME_MASK),
        libc::LC_MESSAGES => (b"LC_MESSAGES", libc::LC_MESSAGES_MASK),
        _ => return None,
    })
}

/// `LC_GLOBAL_LOCALE`, `(locale_t)-1` in `<xlocale.h>` and glibc's
/// `<locale.h>` alike.
const GLOBAL: libc::locale_t = std::ptr::without_provenance_mut(usize::MAX);

/// The name of the environment variable `category` is named by, for
/// [`under`]'s caller to read from the request's environment.
pub(crate) fn variable(category: c_int) -> Option<&'static [u8]> {
    self::category(category).map(|(name, _)| name)
}

/// `f` called with libintl reading `category`'s locale as `name`, where
/// one is given and the C library knows it, and the process's global
/// locale otherwise, with the thread's own put back after. libintl asks
/// the thread's `uselocale(3)` locale first and the process's after it
/// (gnulib's `gl_locale_name_thread` and `gl_locale_name_posix`), so a
/// locale installed here for the call is the one it reads, and the
/// global one sends it where php's own build sends it.
pub(crate) fn under<R>(category: c_int, name: Option<&CStr>, f: impl FnOnce() -> R) -> R {
    let mask = self::category(category).map_or(0, |(_, mask)| mask);
    let own = match name {
        // SAFETY: `name` is NUL-terminated and alive for the call; a
        // null base asks for a fresh locale, which the caller frees.
        Some(name) if mask != 0 => unsafe {
            libc::newlocale(mask, name.as_ptr(), std::ptr::null_mut())
        },
        _ => std::ptr::null_mut(),
    };
    let installed = if own.is_null() { GLOBAL } else { own };
    // SAFETY: `installed` is the global locale or one `newlocale` made
    // and nothing frees until after the thread's own is put back.
    let previous = unsafe { libc::uselocale(installed) };
    let out = f();
    // SAFETY: `previous` is what `uselocale` answered for this thread a
    // moment ago, alive for as long as its owner, the request, is.
    unsafe { libc::uselocale(previous) };
    if !own.is_null() {
        // SAFETY: `own` came from `newlocale` and is installed on no
        // thread now.
        unsafe { libc::freelocale(own) };
    }
    out
}

unsafe extern "C" {
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_textdomain")]
    fn textdomain(domain: *const c_char) -> *mut c_char;
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_gettext")]
    fn gettext(msgid: *const c_char) -> *mut c_char;
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_dgettext")]
    fn dgettext(domain: *const c_char, msgid: *const c_char) -> *mut c_char;
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_dcgettext")]
    fn dcgettext(domain: *const c_char, msgid: *const c_char, category: c_int) -> *mut c_char;
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_dcngettext")]
    fn dcngettext(
        domain: *const c_char,
        msgid1: *const c_char,
        msgid2: *const c_char,
        n: c_ulong,
        category: c_int,
    ) -> *mut c_char;
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_dngettext")]
    fn dngettext(
        domain: *const c_char,
        msgid1: *const c_char,
        msgid2: *const c_char,
        n: c_ulong,
    ) -> *mut c_char;
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_ngettext")]
    fn ngettext(msgid1: *const c_char, msgid2: *const c_char, n: c_ulong) -> *mut c_char;
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_bindtextdomain")]
    fn bindtextdomain(domain: *const c_char, dir: *const c_char) -> *mut c_char;
    #[cfg_attr(target_vendor = "apple", link_name = "libintl_bind_textdomain_codeset")]
    fn bind_textdomain_codeset(domain: *const c_char, codeset: *const c_char) -> *mut c_char;
}

/// A PHP string as the C string `ZSTR_VAL` hands libintl: its bytes up
/// to the first NUL, where C stops reading.
pub(crate) fn c_string(bytes: &[u8]) -> CString {
    CString::new(php_ext::coerce::c_str(bytes)).unwrap_or_default()
}

/// The bytes of the string libintl answered, none for a null pointer.
fn answer(out: *const c_char) -> Option<Vec<u8>> {
    if out.is_null() {
        return None;
    }
    // SAFETY: `out` is non-null and libintl answers a NUL-terminated
    // string, a catalog's, its domain table's or the argument it was
    // handed, each alive until the next call that rebinds it, which the
    // copy here precedes.
    let text = unsafe { CStr::from_ptr(out) };
    Some(text.to_bytes().to_vec())
}

/// `textdomain(domain)`: the domain now current, after setting `domain`
/// where one is given.
pub(crate) fn text_domain(domain: Option<&CStr>) -> Option<Vec<u8>> {
    let domain = domain.map_or(std::ptr::null(), CStr::as_ptr);
    // SAFETY: `domain` is null, which asks, or a NUL-terminated string
    // alive for the call; libintl copies what it keeps.
    let out = unsafe { textdomain(domain) };
    answer(out)
}

/// The domain a lookup reads: the current one (`gettext`), one named
/// (`dgettext`), or one named under a category (`dcgettext`).
#[derive(Clone, Copy)]
pub(crate) enum Domain<'a> {
    Current,
    Named(&'a CStr),
    Category(&'a CStr, c_int),
}

/// `gettext`, `dgettext` or `dcgettext` by `domain`: the translation, or
/// none where libintl answered the `msgid` pointer it was handed, which
/// is how it says it found none.
pub(crate) fn lookup(domain: Domain<'_>, msgid: &CStr) -> Option<Vec<u8>> {
    let id = msgid.as_ptr();
    let out = match domain {
        // SAFETY: `msgid` is NUL-terminated and alive for the call.
        Domain::Current => unsafe { gettext(id) },
        // SAFETY: both strings are NUL-terminated and alive for the call.
        Domain::Named(name) => unsafe { dgettext(name.as_ptr(), id) },
        // SAFETY: both strings are NUL-terminated and alive for the call.
        Domain::Category(name, category) => unsafe { dcgettext(name.as_ptr(), id, category) },
    };
    if out.cast_const() == id {
        return None;
    }
    answer(out)
}

/// `ngettext`, `dngettext` or `dcngettext` by `domain`: the form for
/// `n`, which is one of the two handed in where no catalog has one.
pub(crate) fn plural(domain: Domain<'_>, one: &CStr, many: &CStr, n: c_ulong) -> Vec<u8> {
    let (one, many) = (one.as_ptr(), many.as_ptr());
    let out = match domain {
        // SAFETY: both strings are NUL-terminated and alive for the call.
        Domain::Current => unsafe { ngettext(one, many, n) },
        // SAFETY: the three strings are NUL-terminated and alive for the
        // call.
        Domain::Named(name) => unsafe { dngettext(name.as_ptr(), one, many, n) },
        // SAFETY: the three strings are NUL-terminated and alive for the
        // call.
        Domain::Category(name, category) => unsafe {
            dcngettext(name.as_ptr(), one, many, n, category)
        },
    };
    answer(out).unwrap_or_default()
}

/// `bindtextdomain(domain, dir)`: the directory now bound, after binding
/// `dir` where one is given; none where libintl answers null.
pub(crate) fn bind(domain: &CStr, dir: Option<&CStr>) -> Option<Vec<u8>> {
    let dir = dir.map_or(std::ptr::null(), CStr::as_ptr);
    // SAFETY: `domain` is NUL-terminated, `dir` null or NUL-terminated,
    // each alive for the call; libintl copies what it keeps.
    let out = unsafe { bindtextdomain(domain.as_ptr(), dir) };
    answer(out)
}

/// `bind_textdomain_codeset(domain, codeset)`: the codeset now bound,
/// none where libintl answers null, as it does for a domain with none.
pub(crate) fn bind_codeset(domain: &CStr, codeset: Option<&CStr>) -> Option<Vec<u8>> {
    let codeset = codeset.map_or(std::ptr::null(), CStr::as_ptr);
    // SAFETY: `domain` is NUL-terminated, `codeset` null or
    // NUL-terminated, each alive for the call; libintl copies what it
    // keeps.
    let out = unsafe { bind_textdomain_codeset(domain.as_ptr(), codeset) };
    answer(out)
}
