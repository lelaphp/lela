//! A request's locale as the C library holds it (design §5.14, revision
//! 40): a POSIX-2008 `locale_t` that `newlocale(3)` builds from the
//! categories `setlocale()` names, owned by [`Locale`] and freed when it
//! drops, and installed on the thread serving the request by
//! [`Installed`] with `uselocale(3)`, which puts the thread's own back
//! when it drops. What reads the locale here, `strcoll(3)`,
//! `nl_langinfo(3)` and `localeconv(3)`, is the C library's own code
//! under whatever the calling thread has installed, so a request reads
//! its own locale and never another's, and the process's locale, `C`
//! with the `LC_CTYPE` `php_module_startup` set, is what a thread has
//! before any request installs one. The byte-at-a-time readers of
//! `LC_CTYPE`, `<ctype.h>`'s classes and `toupper(3)` in [`is`] and
//! [`to_upper`] and `php_mblen` in [`Mblen`], are the C library's too.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::{self, NonNull};

/// One category of the locale, as `setlocale()`'s `$category` names it
/// and as `newlocale(3)`'s mask reads it: the mask bits differ between
/// macOS and glibc, so the category is named and the platform's bit
/// read off it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Category {
    Collate,
    Ctype,
    Monetary,
    Numeric,
    Time,
    Messages,
}

impl Category {
    /// The category's `LC_*_MASK`.
    fn mask(self) -> c_int {
        match self {
            Category::Collate => libc::LC_COLLATE_MASK,
            Category::Ctype => libc::LC_CTYPE_MASK,
            Category::Monetary => libc::LC_MONETARY_MASK,
            Category::Numeric => libc::LC_NUMERIC_MASK,
            Category::Time => libc::LC_TIME_MASK,
            Category::Messages => libc::LC_MESSAGES_MASK,
        }
    }
}

/// An owned `locale_t`, from `duplocale(3)` or `newlocale(3)`, which
/// `freelocale(3)` releases when it drops. Never `LC_GLOBAL_LOCALE`,
/// which no one owns.
#[derive(Debug)]
pub(crate) struct Locale(NonNull<c_void>);

// SAFETY: a `locale_t` is an object of the C library that no thread owns:
// POSIX lets any thread install, read or free it, and `Locale` is its one
// owner, so moving it moves nothing a thread holds.
unsafe impl Send for Locale {}

impl Locale {
    /// A copy of the locale the calling thread runs under, the request's
    /// where one is installed and the process's otherwise:
    /// `duplocale(uselocale(NULL))`. `None` where the C library is out of
    /// memory.
    pub(crate) fn current() -> Option<Locale> {
        // SAFETY: a null argument asks for the thread's locale and changes
        // nothing.
        let current = unsafe { libc::uselocale(ptr::null_mut()) };
        // SAFETY: `current` is the thread's installed locale, live while
        // it is installed, or `LC_GLOBAL_LOCALE`, which `duplocale` copies
        // too; the copy is a new object this value owns.
        NonNull::new(unsafe { libc::duplocale(current) }).map(Locale)
    }

    /// `newlocale(mask, name, self)`: this locale with `category` taken
    /// from the locale `name` names, or this locale handed back where the
    /// C library knows no such locale, as `newlocale` leaves its base.
    pub(crate) fn with(self, category: Category, name: &CStr) -> Result<Locale, Locale> {
        let base = self.0.as_ptr();
        // SAFETY: `base` is a locale this value owns and `name` a
        // NUL-terminated string that outlives the call; on success
        // `newlocale` has taken `base` over and the answer is the one
        // object to free, and on failure `base` is untouched.
        let built = unsafe { libc::newlocale(category.mask(), name.as_ptr(), base) };
        match NonNull::new(built) {
            Some(built) => {
                #[expect(
                    clippy::mem_forget,
                    reason = "`newlocale` has taken the base over, so this value's `Drop` would free it a second time"
                )]
                std::mem::forget(self);
                Ok(Locale(built))
            }
            None => Err(self),
        }
    }
}

impl Drop for Locale {
    fn drop(&mut self) {
        // SAFETY: the locale is owned by this value alone and is not
        // installed on any thread: an `Installed` puts the thread's own
        // back before the locale it holds drops.
        unsafe {
            libc::freelocale(self.0.as_ptr());
        }
    }
}

/// The thread's locale an [`Installed`] displaced, `LC_GLOBAL_LOCALE`
/// or whatever the thread had, restored as it was found.
#[derive(Debug)]
struct Displaced(*mut c_void);

// SAFETY: the pointer is only handed back to `uselocale` on the thread
// that serves the request, which holds the request whole from its start
// to its end (design §5.14); nothing reads through it.
unsafe impl Send for Displaced {}

/// A request's locale installed on the thread serving it with
/// `uselocale(3)`: the thread's own is put back when this drops, before
/// the locale is freed.
#[derive(Debug)]
pub(crate) struct Installed {
    locale: Locale,
    displaced: Displaced,
}

impl Installed {
    /// Install `locale` on the calling thread, keeping what the thread
    /// had.
    pub(crate) fn install(locale: Locale) -> Installed {
        // SAFETY: `locale` is a live locale `Installed` keeps alive for as
        // long as it stays installed.
        let displaced = unsafe { libc::uselocale(locale.0.as_ptr()) };
        Installed {
            locale,
            displaced: Displaced(displaced),
        }
    }

    /// Install `locale` in place of the one installed, which is freed
    /// once the thread no longer runs under it.
    pub(crate) fn replace(&mut self, locale: Locale) {
        // SAFETY: as in `install`; the locale it displaces is this
        // value's own, dropped below once nothing runs under it.
        unsafe {
            libc::uselocale(locale.0.as_ptr());
        }
        self.locale = locale;
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        // SAFETY: `displaced` is what this thread ran under before
        // `install`, still live: the thread's own or `LC_GLOBAL_LOCALE`.
        unsafe {
            libc::uselocale(self.displaced.0);
        }
    }
}

/// The bytes a C string holds, empty for a null pointer.
fn bytes(text: *const c_char) -> Vec<u8> {
    if text.is_null() {
        return Vec::new();
    }
    // SAFETY: the C library answered a NUL-terminated string that stays
    // live until the thread's locale next changes, which it cannot
    // before this copy is taken.
    unsafe { CStr::from_ptr(text) }.to_bytes().to_vec()
}

/// `strcoll(3)` under the thread's `LC_COLLATE`.
pub(crate) fn strcoll(left: &CStr, right: &CStr) -> c_int {
    // SAFETY: both are NUL-terminated strings that outlive the call,
    // which reads them and nothing else.
    unsafe { libc::strcoll(left.as_ptr(), right.as_ptr()) }
}

/// `nl_langinfo(3)` under the thread's locale.
pub(crate) fn langinfo(item: libc::nl_item) -> Vec<u8> {
    // SAFETY: `nl_langinfo` takes any item and answers a string for it,
    // empty for one it does not know.
    bytes(unsafe { libc::nl_langinfo(item) })
}

/// `*localeconv()->decimal_point`, the first byte of the thread's
/// `LC_NUMERIC` radix character, which `php_sprintf_appenddouble` writes
/// for `%f`, `%g` and `%G`; read as `nl_langinfo(RADIXCHAR)`, the same
/// field, since glibc's `localeconv` fills one static struct for every
/// thread.
pub(crate) fn decimal_point() -> u8 {
    langinfo(libc::RADIXCHAR).first().copied().unwrap_or(b'.')
}

/// `struct lconv` as `localeconv(3)` answers it under the thread's
/// locale: its eight strings and eight `char`s in the struct's order,
/// and the two groupings.
#[derive(Debug)]
pub(crate) struct Conventions {
    pub(crate) decimal_point: Vec<u8>,
    pub(crate) thousands_sep: Vec<u8>,
    pub(crate) int_curr_symbol: Vec<u8>,
    pub(crate) currency_symbol: Vec<u8>,
    pub(crate) mon_decimal_point: Vec<u8>,
    pub(crate) mon_thousands_sep: Vec<u8>,
    pub(crate) positive_sign: Vec<u8>,
    pub(crate) negative_sign: Vec<u8>,
    pub(crate) int_frac_digits: c_char,
    pub(crate) frac_digits: c_char,
    pub(crate) p_cs_precedes: c_char,
    pub(crate) p_sep_by_space: c_char,
    pub(crate) n_cs_precedes: c_char,
    pub(crate) n_sep_by_space: c_char,
    pub(crate) p_sign_posn: c_char,
    pub(crate) n_sign_posn: c_char,
    pub(crate) grouping: Vec<u8>,
    pub(crate) mon_grouping: Vec<u8>,
}

/// `localeconv()`, which macOS answers from the thread's locale object.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn conventions() -> Conventions {
    // SAFETY: `localeconv` takes nothing and answers the thread's
    // locale's own `struct lconv`, live until that locale next changes.
    let lconv = unsafe { libc::localeconv() };
    // SAFETY: the C library never answers a null `lconv`; the fields
    // are read here, before anything can change the locale.
    let Some(lconv) = (unsafe { lconv.as_ref() }) else {
        return Conventions::c();
    };
    Conventions {
        decimal_point: bytes(lconv.decimal_point),
        thousands_sep: bytes(lconv.thousands_sep),
        int_curr_symbol: bytes(lconv.int_curr_symbol),
        currency_symbol: bytes(lconv.currency_symbol),
        mon_decimal_point: bytes(lconv.mon_decimal_point),
        mon_thousands_sep: bytes(lconv.mon_thousands_sep),
        positive_sign: bytes(lconv.positive_sign),
        negative_sign: bytes(lconv.negative_sign),
        int_frac_digits: lconv.int_frac_digits,
        frac_digits: lconv.frac_digits,
        p_cs_precedes: lconv.p_cs_precedes,
        p_sep_by_space: lconv.p_sep_by_space,
        n_cs_precedes: lconv.n_cs_precedes,
        n_sep_by_space: lconv.n_sep_by_space,
        p_sign_posn: lconv.p_sign_posn,
        n_sign_posn: lconv.n_sign_posn,
        grouping: bytes(lconv.grouping),
        mon_grouping: bytes(lconv.mon_grouping),
    }
}

/// `localeconv()` as glibc builds it, field by field from
/// `nl_langinfo`'s `LC_NUMERIC` and `LC_MONETARY` items
/// (`_NL_ITEM(category, index)`, `<langinfo.h>`), since glibc's own
/// `localeconv` writes one static struct every thread shares.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn conventions() -> Conventions {
    let numeric = |index: libc::nl_item| langinfo((libc::LC_NUMERIC << 16) | index);
    let monetary = |index: libc::nl_item| langinfo((libc::LC_MONETARY << 16) | index);
    let byte = |index: libc::nl_item| {
        monetary(index)
            .first()
            .map_or(c_char::MAX, |b| c_char::from_ne_bytes([*b]))
    };
    Conventions {
        decimal_point: numeric(0),
        thousands_sep: numeric(1),
        grouping: numeric(2),
        int_curr_symbol: monetary(0),
        currency_symbol: monetary(1),
        mon_decimal_point: monetary(2),
        mon_thousands_sep: monetary(3),
        mon_grouping: monetary(4),
        positive_sign: monetary(5),
        negative_sign: monetary(6),
        int_frac_digits: byte(7),
        frac_digits: byte(8),
        p_cs_precedes: byte(9),
        p_sep_by_space: byte(10),
        n_cs_precedes: byte(11),
        n_sep_by_space: byte(12),
        p_sign_posn: byte(13),
        n_sign_posn: byte(14),
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
impl Conventions {
    /// The C locale's, for a C library that answered no `lconv`.
    fn c() -> Conventions {
        Conventions {
            decimal_point: b".".to_vec(),
            thousands_sep: Vec::new(),
            int_curr_symbol: Vec::new(),
            currency_symbol: Vec::new(),
            mon_decimal_point: Vec::new(),
            mon_thousands_sep: Vec::new(),
            positive_sign: Vec::new(),
            negative_sign: Vec::new(),
            int_frac_digits: c_char::MAX,
            frac_digits: c_char::MAX,
            p_cs_precedes: c_char::MAX,
            p_sep_by_space: c_char::MAX,
            n_cs_precedes: c_char::MAX,
            n_sep_by_space: c_char::MAX,
            p_sign_posn: c_char::MAX,
            n_sign_posn: c_char::MAX,
            grouping: Vec::new(),
            mon_grouping: Vec::new(),
        }
    }
}

/// One of `<ctype.h>`'s classes that ext/standard's natives read of a
/// byte under the thread's `LC_CTYPE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Alnum,
    Alpha,
    Digit,
    Punct,
}

/// `isalnum((unsigned char) c)` and the rest: whether `byte` is in
/// `class` under the thread's `LC_CTYPE`.
pub(crate) fn is(class: Class, byte: u8) -> bool {
    let test: unsafe extern "C" fn(c_int) -> c_int = match class {
        Class::Alnum => libc::isalnum,
        Class::Alpha => libc::isalpha,
        Class::Digit => libc::isdigit,
        Class::Punct => libc::ispunct,
    };
    // SAFETY: every class function takes any value an `unsigned char`
    // holds and reads the thread's locale, nothing else.
    unsafe { test(c_int::from(byte)) != 0 }
}

/// `toupper((unsigned char) c)` under the thread's `LC_CTYPE`, as the
/// `unsigned char` php stores it in.
pub(crate) fn to_upper(byte: u8) -> u8 {
    // SAFETY: `toupper` takes any value an `unsigned char` holds and
    // reads the thread's locale, nothing else.
    let upper = unsafe { libc::toupper(c_int::from(byte)) };
    upper.to_le_bytes()[0]
}

/// `mbstate_t`, as large and as aligned as the largest the platforms
/// lay out (macOS's 128-byte union with a `long long`; glibc's is 8
/// bytes): the C library reads and writes it and nothing here does.
#[repr(C, align(8))]
struct MbState([u8; 128]);

unsafe extern "C" {
    /// `mbrlen(3)`, which the `libc` crate does not declare.
    fn mbrlen(s: *const c_char, n: usize, ps: *mut MbState) -> usize;
    /// `MB_CUR_MAX` as macOS's and FreeBSD's `<stdlib.h>` define it.
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    fn ___mb_cur_max() -> c_int;
    /// `MB_CUR_MAX` as glibc's and musl's `<stdlib.h>` define it.
    #[cfg(not(any(target_vendor = "apple", target_os = "freebsd")))]
    fn __ctype_get_mb_cur_max() -> usize;
}

/// `MB_CUR_MAX`: the most bytes a character takes under the thread's
/// `LC_CTYPE`.
fn mb_cur_max() -> usize {
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    // SAFETY: takes nothing and reads the thread's locale.
    let max = usize::try_from(unsafe { ___mb_cur_max() }).unwrap_or(1);
    #[cfg(not(any(target_vendor = "apple", target_os = "freebsd")))]
    // SAFETY: takes nothing and reads the thread's locale.
    let max = unsafe { __ctype_get_mb_cur_max() };
    max
}

/// `CG(ascii_compatible_locale)` as `zend_update_current_locale`
/// (`Z/Zend/zend_operators.c:2923`) computes it each time `LC_CTYPE`
/// moves, read here off the thread's locale when asked: every character
/// one byte, or a multibyte locale whose `CODESET` is UTF-8.
pub(crate) fn ascii_compatible() -> bool {
    if mb_cur_max() <= 1 {
        return true;
    }
    let charmap = langinfo(libc::CODESET);
    charmap.eq_ignore_ascii_case(b"utf-8") || charmap.eq_ignore_ascii_case(b"utf8")
}

/// `php_mblen` over one walk, the `mbrlen(3)` arm php's `php_string.h`
/// takes on a threaded build: the length of the character at the start
/// of a string under the thread's `LC_CTYPE`, with the shift state
/// between calls held here, all zero at the start as `php_mb_reset`'s
/// `memset` leaves `BG(mblen_state)`.
pub(crate) struct Mblen(MbState);

impl Mblen {
    /// The state `php_mb_reset` leaves.
    pub(crate) fn new() -> Mblen {
        Mblen(MbState([0; 128]))
    }

    /// `php_mb_reset()`.
    pub(crate) fn reset(&mut self) {
        self.0 = MbState([0; 128]);
    }

    /// `(int) mbrlen(ptr, len, &state)`: the bytes the character at the
    /// start of `bytes` takes, 0 for a NUL, -1 for no valid character
    /// and -2 for one `bytes` ends inside.
    pub(crate) fn len(&mut self, bytes: &[u8]) -> isize {
        let ptr = bytes.as_ptr().cast::<c_char>();
        // SAFETY: `mbrlen` reads at most `bytes.len()` bytes from `ptr`,
        // all of them live, and writes the state this value owns.
        let len = unsafe { mbrlen(ptr, bytes.len(), &raw mut self.0) };
        // `(int) mbrlen(...)`: `(size_t) -1` and `-2` are -1 and -2.
        isize::from_ne_bytes(len.to_ne_bytes())
    }
}
