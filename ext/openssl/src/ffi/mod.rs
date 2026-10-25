//! The binding to OpenSSL's libcrypto (design §7.2, `unsafe.homes`'
//! `ext/*/src/ffi/`): the platform's OpenSSL of the major version the
//! oracle links, 4 since php 8.5.12 (#3381), found by `openssl-sys` through `pkg-config`, and the one
//! home of this crate's `unsafe`. What `openssl-sys` does not declare is
//! declared beside its use from the library's own headers.
//!
//! One owned type per OpenSSL object, each freeing what it owns on drop,
//! so no native frees by hand: the keys and their operations in [`key`],
//! the digests and ciphers in [`cipher`], certificates, signing requests,
//! names, stores and SPKACs in [`x509`], and PKCS#7, CMS and PKCS#12 in
//! [`smime`]; this file holds what they share, the `BIO`, the `CONF`,
//! stacks, object names and the error queue. Every call is made on the
//! default `OSSL_LIB_CTX` with no property query; the oracle's
//! `openssl.libctx=custom` `OSSL_LIB_CTX` is one loaded from the same
//! configuration and has the same providers, so what the natives answer
//! does not follow from which one is used.
//!
//! The library's error queue is per thread, and a request runs on one
//! thread, so [`next_error`] drains what the calls of the native that asks
//! left behind, as `php_openssl_store_errors` does.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uchar, c_ulong, c_void};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};

use openssl_sys as sys;

/// One owned OpenSSL object: a non-null pointer the type frees on drop
/// with the library's own free function, and nowhere else.
macro_rules! owned {
    ($(#[$doc:meta])* $name:ident, $ty:ty, $free:path) => {
        $(#[$doc])*
        pub(crate) struct $name(::std::ptr::NonNull<$ty>);

        impl Drop for $name {
            fn drop(&mut self) {
                // SAFETY: the object is owned here and freed once.
                unsafe { $free(self.0.as_ptr()) };
            }
        }

        #[allow(dead_code)]
        impl $name {
            pub(in crate::ffi) fn owned(ptr: *mut $ty) -> Option<$name> {
                ::std::ptr::NonNull::new(ptr).map($name)
            }

            pub(in crate::ffi) fn as_ptr(&self) -> *mut $ty {
                self.0.as_ptr()
            }
        }
    };
}

pub(crate) mod cipher;
pub(crate) mod key;
pub(crate) mod smime;
pub(crate) mod ssl;
pub(crate) mod x509;

pub(crate) use cipher::{Cipher, Digest};
pub(crate) use key::{KeyKind, PKey};
pub(crate) use x509::Certificate;

// SAFETY: libcrypto's own declarations, from `openssl/err.h`,
// `openssl/conf.h`, `openssl/objects.h`, `openssl/x509.h`,
// `openssl/bio.h` and `openssl/rand.h` of OpenSSL 3, which `openssl-sys`
// does not declare; the library `openssl-sys` links defines each.
unsafe extern "C" {
    fn ERR_set_mark() -> c_int;
    fn ERR_pop_to_mark() -> c_int;
    fn ERR_error_string_n(e: c_ulong, buf: *mut c_char, len: usize);
    fn NCONF_load(conf: *mut sys::CONF, file: *const c_char, eline: *mut c_long) -> c_int;
    fn NCONF_get_string(
        conf: *const sys::CONF,
        group: *const c_char,
        name: *const c_char,
    ) -> *mut c_char;
    fn NCONF_get_number_e(
        conf: *const sys::CONF,
        group: *const c_char,
        name: *const c_char,
        result: *mut c_long,
    ) -> c_int;
    fn NCONF_get_section(conf: *const sys::CONF, section: *const c_char)
    -> *mut sys::OPENSSL_STACK;
    fn OBJ_txt2nid(s: *const c_char) -> c_int;
    fn OBJ_ln2nid(s: *const c_char) -> c_int;
    fn OBJ_create(oid: *const c_char, sn: *const c_char, ln: *const c_char) -> c_int;
    fn OBJ_create_objects(input: *mut sys::BIO) -> c_int;
    fn ASN1_STRING_set_default_mask_asc(p: *const c_char) -> c_int;
    fn X509_get_default_cert_area() -> *const c_char;
    fn X509_get_default_cert_file() -> *const c_char;
    fn X509_get_default_cert_file_env() -> *const c_char;
    fn X509_get_default_cert_dir() -> *const c_char;
    fn X509_get_default_cert_dir_env() -> *const c_char;
    fn X509_get_default_private_dir() -> *const c_char;
    fn BIO_new_file(filename: *const c_char, mode: *const c_char) -> *mut sys::BIO;
    fn BIO_free(a: *mut sys::BIO) -> c_int;
    fn RAND_file_name(buf: *mut c_char, num: usize) -> *const c_char;
    fn RAND_load_file(file: *const c_char, max_bytes: c_long) -> c_int;
    fn RAND_write_file(file: *const c_char) -> c_int;
}

/// `BIO_CTRL_RESET`, the control `BIO_reset` is.
const BIO_CTRL_RESET: c_int = 1;

/// `ERR_get_error()`: the oldest code on this thread's queue, taken off
/// it, or `None` once the queue is empty.
pub(crate) fn next_error() -> Option<c_ulong> {
    // SAFETY: ERR_get_error reads and pops this thread's queue; it takes
    // nothing.
    let code = unsafe { sys::ERR_get_error() };
    (code != 0).then_some(code)
}

/// `OPENSSL_INIT_LOAD_CONFIG`, which openssl-sys does not carry.
const OPENSSL_INIT_LOAD_CONFIG: u64 = 0x0000_0040;

/// `php_openssl_backend_init`'s `OPENSSL_init_ssl(OPENSSL_INIT_LOAD_CONFIG,
/// NULL)`: the library started with its configuration file read and every
/// digest and cipher entered in the name table `openssl_get_md_methods`
/// and `openssl_get_cipher_methods` walk. One time per process, whoever asks.
pub(crate) fn start() {
    // SAFETY: OPENSSL_init_ssl is safe to call from any thread any number
    // of times; NULL settings are the defaults.
    unsafe { sys::OPENSSL_init_ssl(OPENSSL_INIT_LOAD_CONFIG, std::ptr::null()) };
}

/// `ERR_clear_error()`: this thread's queue emptied, of what a request
/// the thread served before left on it.
pub(crate) fn clear_errors() {
    // SAFETY: ERR_clear_error empties this thread's queue; it takes
    // nothing.
    unsafe { sys::ERR_clear_error() };
}

/// `ERR_error_string_n(code, buf, 256)`: the library's text for a code,
/// `error:0480006C:PEM routines::no start line` and its kin, cut at 255
/// bytes as php's buffer cuts it.
pub(crate) fn error_text(code: c_ulong) -> Vec<u8> {
    let mut buf = [0 as c_char; 256];
    // SAFETY: the buffer is 256 bytes and the length says so; the library
    // writes at most that many, NUL included.
    unsafe { ERR_error_string_n(code, buf.as_mut_ptr(), buf.len()) };
    // SAFETY: ERR_error_string_n NUL-terminates within the length given.
    let text = unsafe { CStr::from_ptr(buf.as_ptr()) };
    text.to_bytes().to_vec()
}

/// `ERR_set_mark()` and `ERR_pop_to_mark()` around `run`: what the calls
/// in it queue is dropped, as php's optional configuration reads drop it.
pub(in crate::ffi) fn quietly<T>(run: impl FnOnce() -> T) -> T {
    // SAFETY: ERR_set_mark marks this thread's queue; it takes nothing.
    unsafe { ERR_set_mark() };
    let out = run();
    // SAFETY: ERR_pop_to_mark pops this thread's queue to the mark set
    // above; it takes nothing.
    unsafe { ERR_pop_to_mark() };
    out
}

/// A C string the library hands out and keeps, copied.
///
/// # Safety
///
/// `text` is null or a NUL-terminated string that lives for the call.
pub(in crate::ffi) unsafe fn copied(text: *const c_char) -> Option<Vec<u8>> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the caller's contract: non-null and NUL-terminated.
    let text = unsafe { CStr::from_ptr(text) };
    Some(text.to_bytes().to_vec())
}

/// A C string the library allocated for the caller, copied and freed
/// with `OPENSSL_free`.
///
/// # Safety
///
/// `text` is null or a NUL-terminated string the caller owns.
pub(in crate::ffi) unsafe fn taken(text: *mut c_char) -> Option<Vec<u8>> {
    // SAFETY: the caller's contract, as `copied` asks it.
    let out = unsafe { copied(text) };
    if !text.is_null() {
        // SAFETY: the string is the caller's to free, once.
        unsafe { sys::OPENSSL_free(text.cast()) };
    }
    out
}

/// `len` bytes at `data`, copied; empty for a null pointer.
///
/// # Safety
///
/// `data` is null or points at `len` readable bytes.
pub(in crate::ffi) unsafe fn bytes_at(data: *const c_uchar, len: c_int) -> Vec<u8> {
    let len = usize::try_from(len).unwrap_or(0);
    if data.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: the caller's contract: `len` readable bytes at `data`.
    unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
}

/// A length as the `int` the library takes; `None` past `INT_MAX`.
pub(in crate::ffi) fn int_len(len: usize) -> Option<c_int> {
    c_int::try_from(len).ok()
}

/// The library's two-call convention for bytes of unknown length: a
/// null buffer asks for the most it writes, a buffer of that many bytes
/// takes what it wrote. `None` where either call answers `<= 0`.
pub(in crate::ffi) fn two_calls(
    mut call: impl FnMut(*mut c_uchar, &mut usize) -> c_int,
) -> Option<Vec<u8>> {
    let mut len = 0usize;
    if call(ptr::null_mut(), &mut len) <= 0 {
        return None;
    }
    let mut out = vec![0u8; len];
    if call(out.as_mut_ptr(), &mut len) <= 0 {
        return None;
    }
    out.truncate(len);
    Some(out)
}

/// `OpenSSL_version(OPENSSL_VERSION)`: the loaded libcrypto's version
/// line, `OpenSSL 3.6.4 25 Aug 2026`, which `--ri` prints as "Library
/// Version"; the header's is `OPENSSL_VERSION_TEXT`.
pub(crate) fn version_text() -> Vec<u8> {
    // SAFETY: OpenSSL_version answers a pointer into a static string.
    let text = unsafe { sys::OpenSSL_version(sys::OPENSSL_VERSION) };
    // SAFETY: the static string is NUL-terminated and never freed.
    unsafe { copied(text) }.unwrap_or_default()
}

/// The directories and variables the library was built to look in, as
/// `php_openssl_set_cert_locations` lists them: the default certificate
/// file and its variable, the directory and its variable, the private
/// directory and the certificate area.
pub(crate) fn cert_locations() -> [Vec<u8>; 6] {
    let get = |f: unsafe extern "C" fn() -> *const c_char| {
        // SAFETY: each of these answers a static string the library keeps.
        let text = unsafe { f() };
        // SAFETY: the static string is NUL-terminated and never freed.
        unsafe { copied(text) }.unwrap_or_default()
    };
    [
        get(X509_get_default_cert_file),
        get(X509_get_default_cert_file_env),
        get(X509_get_default_cert_dir),
        get(X509_get_default_cert_dir_env),
        get(X509_get_default_private_dir),
        get(X509_get_default_cert_area),
    ]
}

/// `X509_get_default_cert_area()`: the directory the library was
/// configured with, where `openssl.cnf` is looked for by default.
pub(crate) fn default_cert_area() -> Vec<u8> {
    cert_locations()[5].clone()
}

/// `OBJ_sn2nid(name)`: the NID of a short name, `NID_undef` (0) for one
/// the library does not know.
pub(crate) fn short_name_nid(name: &CStr) -> c_int {
    // SAFETY: the name is a NUL-terminated string for the call.
    unsafe { sys::OBJ_sn2nid(name.as_ptr()) }
}

/// `OBJ_ln2nid(name)`: the NID of a long name, 0 for none.
pub(crate) fn long_name_nid(name: &CStr) -> c_int {
    // SAFETY: the name is a NUL-terminated string for the call.
    unsafe { OBJ_ln2nid(name.as_ptr()) }
}

/// `OBJ_txt2nid(text)`: the NID a short name, long name or dotted OID
/// names, 0 for none.
pub(crate) fn text_nid(text: &CStr) -> c_int {
    // SAFETY: the text is a NUL-terminated string for the call.
    unsafe { OBJ_txt2nid(text.as_ptr()) }
}

/// `OBJ_nid2sn(nid)`: a NID's short name.
pub(crate) fn nid_short_name(nid: c_int) -> Option<Vec<u8>> {
    // SAFETY: OBJ_nid2sn takes any NID and answers a static name or null.
    let name = unsafe { sys::OBJ_nid2sn(nid) };
    // SAFETY: a non-null name is the library's static string.
    unsafe { copied(name) }
}

/// `OBJ_nid2ln(nid)`: a NID's long name.
pub(crate) fn nid_long_name(nid: c_int) -> Option<Vec<u8>> {
    // SAFETY: OBJ_nid2ln takes any NID and answers a static name or null.
    let name = unsafe { sys::OBJ_nid2ln(nid) };
    // SAFETY: a non-null name is the library's static string.
    unsafe { copied(name) }
}

/// `OBJ_create(oid, sn, ln)`: the NID of the object made, 0 where the
/// library refused.
pub(crate) fn create_object(oid: &CStr, short: &CStr, long: &CStr) -> c_int {
    // SAFETY: the three strings are NUL-terminated for the call.
    unsafe { OBJ_create(oid.as_ptr(), short.as_ptr(), long.as_ptr()) }
}

/// `OBJ_create_objects(in)`: every `oid short long` line of a file made
/// an object.
pub(crate) fn create_objects(bio: &Bio<'_>) {
    // SAFETY: the BIO is live.
    unsafe { OBJ_create_objects(bio.as_ptr()) };
}

/// `OBJ_obj2txt(buf, size, obj, 1)`: an object's dotted OID, cut to
/// `size - 1` bytes as the library cuts it.
///
/// # Safety
///
/// `object` is a live `ASN1_OBJECT`.
pub(in crate::ffi) unsafe fn object_oid(object: *const sys::ASN1_OBJECT, size: usize) -> Vec<u8> {
    let mut buf = vec![0 as c_char; size.max(1)];
    let len = int_len(buf.len()).unwrap_or(0);
    // SAFETY: the caller's live object, and a buffer of `len` bytes.
    unsafe { sys::OBJ_obj2txt(buf.as_mut_ptr(), len, object, 1) };
    // SAFETY: OBJ_obj2txt NUL-terminates within the length given.
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_bytes().to_vec()
}

/// `OBJ_obj2txt(buf, 80, OBJ_nid2obj(nid), 1)`: the dotted OID of a NID,
/// in the 80 bytes OpenSSL recommends.
pub(crate) fn nid_oid(nid: c_int) -> Option<Vec<u8>> {
    // SAFETY: OBJ_nid2obj takes any NID and answers a new object or null.
    let object = unsafe { sys::OBJ_nid2obj(nid) };
    if object.is_null() {
        return None;
    }
    // SAFETY: the object is live until it is freed below.
    let oid = unsafe { object_oid(object, 80) };
    // SAFETY: the object came from OBJ_nid2obj above and is freed once.
    unsafe { sys::ASN1_OBJECT_free(object) };
    Some(oid)
}

/// `ASN1_STRING_set_default_mask_asc(mask)`: the process's default string
/// mask, which a configuration's `string_mask` sets; false for a mask
/// the library refuses.
pub(crate) fn set_default_string_mask(mask: &CStr) -> bool {
    // SAFETY: the mask is a NUL-terminated string for the call.
    unsafe { ASN1_STRING_set_default_mask_asc(mask.as_ptr()) != 0 }
}

/// `RAND_file_name(buf, MAXPATHLEN)`: the seed file the library names,
/// `$RANDFILE` or `$HOME/.rnd`.
pub(crate) fn rand_file_name() -> Option<CString> {
    let mut buf = [0 as c_char; 1024];
    // SAFETY: the buffer is 1024 bytes and the length says so; the
    // library answers the buffer or null.
    let name = unsafe { RAND_file_name(buf.as_mut_ptr(), buf.len()) };
    // SAFETY: a non-null answer is the buffer, NUL-terminated.
    unsafe { copied(name) }.and_then(|name| CString::new(name).ok())
}

/// `RAND_load_file(file, -1)`: the bytes read into the generator's
/// state, negative for a file it could not read.
pub(crate) fn rand_load_file(file: &CStr) -> c_int {
    // SAFETY: the name is a NUL-terminated string for the call.
    unsafe { RAND_load_file(file.as_ptr(), -1) }
}

/// `RAND_write_file(file)`: negative for a file it could not write.
pub(crate) fn rand_write_file(file: &CStr) -> c_int {
    // SAFETY: the name is a NUL-terminated string for the call.
    unsafe { RAND_write_file(file.as_ptr()) }
}

/// `RAND_status()`: whether the generator is seeded.
pub(crate) fn rand_status() -> bool {
    // SAFETY: RAND_status takes nothing.
    unsafe { sys::RAND_status() == 1 }
}

/// `RAND_bytes(buf, len)`: `len` bytes from the generator, `None` where
/// it refused.
pub(crate) fn rand_bytes(len: c_int) -> Option<Vec<u8>> {
    let mut out = vec![0u8; usize::try_from(len).ok()?];
    // SAFETY: the buffer holds `len` bytes.
    let ok = unsafe { sys::RAND_bytes(out.as_mut_ptr(), len) };
    (ok > 0).then_some(out)
}

/// A `BIO`: a read-only view of the caller's bytes, a memory sink, or a
/// file, freed on drop. The lifetime is the bytes a view reads.
pub(crate) struct Bio<'a>(NonNull<sys::BIO>, PhantomData<&'a [u8]>);

impl<'a> Bio<'a> {
    /// `BIO_new_mem_buf(bytes, len)`: the bytes, read in place; `None`
    /// past `INT_MAX` bytes, where php's `(int)` cast would ask for
    /// `strlen` of them instead.
    pub(crate) fn reading(bytes: &'a [u8]) -> Option<Bio<'a>> {
        let len = int_len(bytes.len())?;
        // SAFETY: the pointer and length are the slice's, and the BIO
        // reads them only while it lives, which the lifetime ties to them.
        let bio = unsafe { sys::BIO_new_mem_buf(bytes.as_ptr().cast(), len) };
        NonNull::new(bio).map(|bio| Bio(bio, PhantomData))
    }

    /// `BIO_new(BIO_s_mem())`: an empty memory sink.
    pub(crate) fn memory() -> Option<Bio<'static>> {
        // SAFETY: BIO_s_mem takes nothing and answers the static method
        // table.
        let method = unsafe { sys::BIO_s_mem() };
        // SAFETY: the method table is the library's own; BIO_new makes a
        // BIO of it or answers null.
        let bio = unsafe { sys::BIO_new(method) };
        NonNull::new(bio).map(|bio| Bio(bio, PhantomData))
    }

    /// `BIO_new_file(path, mode)`: the file, opened by the C library.
    pub(crate) fn file(path: &CStr, mode: &CStr) -> Option<Bio<'static>> {
        // SAFETY: both strings are NUL-terminated for the call.
        let bio = unsafe { BIO_new_file(path.as_ptr(), mode.as_ptr()) };
        NonNull::new(bio).map(|bio| Bio(bio, PhantomData))
    }

    /// `BIO_get_mem_data`: what a memory sink holds.
    pub(crate) fn contents(&self) -> Vec<u8> {
        let mut data: *mut c_char = ptr::null_mut();
        // SAFETY: the BIO is live; for a memory BIO the control call
        // stores a pointer to its bytes and answers their length.
        let len = unsafe { sys::BIO_get_mem_data(self.0.as_ptr(), &raw mut data) };
        // SAFETY: the BIO's own buffer, `len` bytes, live while the BIO
        // is and not written while this copy reads it.
        unsafe { bytes_at(data.cast(), c_int::try_from(len).unwrap_or(0)) }
    }

    /// `BIO_write(bio, bytes, len) > 0`, or true for nothing to write.
    pub(crate) fn write(&self, bytes: &[u8]) -> bool {
        let Some(len) = int_len(bytes.len()) else {
            return false;
        };
        // SAFETY: the BIO is live and the slice is `len` bytes.
        len == 0 || unsafe { sys::BIO_write(self.0.as_ptr(), bytes.as_ptr().cast(), len) } > 0
    }

    /// `BIO_reset(bio)`: a file back to its start, a memory view to its
    /// first byte.
    pub(crate) fn reset(&self) -> bool {
        // SAFETY: the BIO is live; the reset control takes no argument.
        let done = unsafe { sys::BIO_ctrl(self.0.as_ptr(), BIO_CTRL_RESET, 0, ptr::null_mut()) };
        done == 0
    }

    pub(in crate::ffi) fn as_ptr(&self) -> *mut sys::BIO {
        self.0.as_ptr()
    }

    /// A BIO the library made and handed over, owned from here.
    pub(in crate::ffi) fn adopt(bio: *mut sys::BIO) -> Option<Bio<'static>> {
        NonNull::new(bio).map(|bio| Bio(bio, PhantomData))
    }
}

impl Drop for Bio<'_> {
    fn drop(&mut self) {
        // SAFETY: the BIO is owned here and freed once.
        unsafe { BIO_free(self.0.as_ptr()) };
    }
}

/// What a memory sink holds once `write` has put something in it, or
/// `None` where the sink could not be made or `write` refused.
pub(in crate::ffi) fn written(write: impl FnOnce(*mut sys::BIO) -> c_int) -> Option<Vec<u8>> {
    let out = Bio::memory()?;
    (write(out.as_ptr()) > 0).then(|| out.contents())
}

owned!(
    /// A `CONF`, the parsed form of an `openssl.cnf`.
    Conf,
    sys::CONF,
    sys::NCONF_free
);

impl Conf {
    /// `NCONF_new(NULL)`: an empty configuration of the default method.
    pub(crate) fn new() -> Option<Conf> {
        // SAFETY: a null method asks for the default one.
        Conf::owned(unsafe { sys::NCONF_new(ptr::null_mut()) })
    }

    /// `NCONF_load(conf, file, NULL)`: false for a file the library could
    /// not read or parse, its reasons on the queue.
    pub(crate) fn load(&mut self, file: &CStr) -> bool {
        // SAFETY: the configuration is live and the name NUL-terminated.
        unsafe { NCONF_load(self.as_ptr(), file.as_ptr(), ptr::null_mut()) > 0 }
    }

    /// `NCONF_get_string`, what the lookup queues left on the queue.
    pub(crate) fn string_loud(&self, group: Option<&CStr>, name: &CStr) -> Option<Vec<u8>> {
        let group = group.map_or(ptr::null(), CStr::as_ptr);
        // SAFETY: the configuration is live; the strings are
        // NUL-terminated; the answer is the configuration's own.
        let value = unsafe { NCONF_get_string(self.as_ptr(), group, name.as_ptr()) };
        // SAFETY: a non-null answer is NUL-terminated and lives with the
        // configuration.
        unsafe { copied(value) }
    }

    /// `php_openssl_conf_get_string`: the value of `name` in `group` (or
    /// the default section), what the lookup queues dropped.
    pub(crate) fn string(&self, group: Option<&CStr>, name: &CStr) -> Option<Vec<u8>> {
        quietly(|| self.string_loud(group, name))
    }

    /// `php_openssl_conf_get_number`: the number `name` in `group` holds,
    /// 0 where it holds none, what the lookup queues dropped.
    pub(crate) fn number(&self, group: Option<&CStr>, name: &CStr) -> c_long {
        let group = group.map_or(ptr::null(), CStr::as_ptr);
        quietly(|| {
            let mut out: c_long = 0;
            // SAFETY: the configuration is live; the strings are
            // NUL-terminated; the number is stored in `out` on success
            // and `out` is left alone otherwise.
            unsafe { NCONF_get_number_e(self.as_ptr(), group, name.as_ptr(), &raw mut out) };
            out
        })
    }

    /// `NCONF_get_section(conf, name)`: its `name = value` lines in
    /// order, `None` for a section the file lacks, its reason queued.
    pub(crate) fn section(&self, name: &CStr) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        // SAFETY: the configuration is live and the name NUL-terminated;
        // the stack answered is the configuration's own.
        let stack = unsafe { NCONF_get_section(self.as_ptr(), name.as_ptr()) };
        if stack.is_null() {
            return None;
        }
        // SAFETY: the stack is live while the configuration is.
        let items = unsafe { stack_items(stack) };
        let lines = items.into_iter().map(|item| {
            let value = item.cast::<ConfValue>();
            // SAFETY: each item of a section's stack is a CONF_VALUE whose
            // strings live with the configuration.
            let value = unsafe { &*value };
            let (name, text) = (value.name, value.value);
            // SAFETY: the name is NUL-terminated or null.
            let name = unsafe { copied(name) }.unwrap_or_default();
            // SAFETY: the value is NUL-terminated or null.
            (name, unsafe { copied(text) }.unwrap_or_default())
        });
        Some(lines.collect())
    }
}

/// `CONF_VALUE`, one line of a configuration section.
#[repr(C)]
struct ConfValue {
    section: *mut c_char,
    name: *mut c_char,
    value: *mut c_char,
}

/// Every item of a stack, in order, as the pointers it holds.
///
/// # Safety
///
/// `stack` is a live `OPENSSL_STACK`.
pub(in crate::ffi) unsafe fn stack_items(stack: *const sys::OPENSSL_STACK) -> Vec<*mut c_void> {
    // SAFETY: the caller's live stack.
    let count = unsafe { sys::OPENSSL_sk_num(stack) };
    // SAFETY: each index is below the count.
    (0..count.max(0))
        .map(|i| unsafe { sys::OPENSSL_sk_value(stack, i) })
        .collect()
}
