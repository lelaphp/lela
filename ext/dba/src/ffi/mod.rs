//! The platform's ndbm, `<ndbm.h>` from the C library (design §7.2;
//! `unsafe.homes`' `ext/*/src/ffi/`), the one the oracle's build links:
//! macOS maps it onto `db(3)`'s hash, the database `<path>.db`. `dbm_open`
//! and its kin behind safe functions, and the open database as a value that
//! `dbm_close`s when it is dropped. The one home of this crate's `unsafe`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CString, c_char, c_int, c_void};
use std::ptr::NonNull;

/// `datum`: a pointer and a length, the library's own memory in what it
/// answers, valid until the next call on the database.
#[repr(C)]
struct Datum {
    dptr: *mut c_void,
    dsize: usize,
}

/// `DBM`, opaque.
#[repr(C)]
struct Dbm {
    _opaque: [u8; 0],
}

/// `DBM_INSERT` and `DBM_REPLACE`.
const DBM_INSERT: c_int = 0;
const DBM_REPLACE: c_int = 1;

unsafe extern "C" {
    fn dbm_open(file: *const c_char, flags: c_int, mode: libc::mode_t) -> *mut Dbm;
    fn dbm_close(db: *mut Dbm);
    fn dbm_fetch(db: *mut Dbm, key: Datum) -> Datum;
    fn dbm_store(db: *mut Dbm, key: Datum, content: Datum, flags: c_int) -> c_int;
    fn dbm_delete(db: *mut Dbm, key: Datum) -> c_int;
    fn dbm_firstkey(db: *mut Dbm) -> Datum;
    fn dbm_nextkey(db: *mut Dbm) -> Datum;
}

/// The bytes as a `datum` the library reads and never writes.
const fn datum(bytes: &[u8]) -> Datum {
    Datum {
        dptr: bytes.as_ptr().cast_mut().cast(),
        dsize: bytes.len(),
    }
}

/// What the library answered, copied out before the next call reuses
/// its buffer; `None` for a null `dptr`.
fn copied(answer: &Datum) -> Option<Vec<u8>> {
    if answer.dptr.is_null() {
        return None;
    }
    // SAFETY: a non-null `dptr` the library answered points at `dsize`
    // bytes it owns until the next call on the database, which this frame
    // makes none of before the copy.
    let bytes = unsafe { std::slice::from_raw_parts(answer.dptr.cast::<u8>(), answer.dsize) };
    Some(bytes.to_vec())
}

/// One open database, `info->dbf`.
pub(crate) struct Ndbm {
    db: NonNull<Dbm>,
}

// SAFETY: the value is the library's state for one database, touched by
// no call on any other database; the link that owns it is used by one
// thread at a time.
unsafe impl Send for Ndbm {}

// The `DBM *` is the C library's own state for one database file, its
// buffers in the library's memory: `dbm_open` is handed a path, flags and
// a mode, and every call after copies its key and value in and out
// (`copied`), so nothing of a request's heap reaches it. Not `Sync`, and
// not asked to be: the pool keeps it behind its `Mutex` and only moves it.
impl php_ext::abi::EngineState for Ndbm {}

impl Ndbm {
    /// `dbm_open(path, flags, mode)`, or `None` where the library refused
    /// or the path holds a NUL. The library's file is `<path>.db`; under
    /// the default `$mode` `php_dba_open` takes its `flock(2)` on `<path>`
    /// itself, and under `l` on `<path>.lck`.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/dba_ndbm_open_' . getmypid();
    /// $d = dba_open($f, 'n', 'ndbm');
    /// var_dump($d, dba_list() === [spl_object_id($d) => realpath($f)], dba_handlers(true)['ndbm']);
    /// dba_close($d);
    /// var_dump(file_exists($f), filesize($f), file_exists("$f.db"));
    /// $l = dba_open($f, 'cl', 'ndbm'); var_dump(file_exists("$f.lck")); dba_close($l);
    /// var_dump(dba_open("$f.none", 'r', 'ndbm'));
    /// unlink($f); unlink("$f.db"); unlink("$f.lck");
    /// ```
    /// ```expect
    /// object(Dba\Connection)#1 (0) {
    /// }
    /// bool(true)
    /// string(4) "NDBM"
    /// bool(true)
    /// int(0)
    /// bool(true)
    /// bool(true)
    ///
    /// Warning: dba_open(%s.none): Failed to open stream: No such file or directory in %s on line 8
    /// bool(false)
    /// ```
    pub(crate) fn open(path: &[u8], flags: i32, mode: u16) -> Option<Ndbm> {
        let path = CString::new(path).ok()?;
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        let db = unsafe { dbm_open(path.as_ptr(), flags, mode) };
        NonNull::new(db).map(|db| Ndbm { db })
    }

    /// `dbm_fetch`: the value under the key, for `dba_fetch` and
    /// `dba_exists`; the skip ignored with php's notice.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/dba_ndbm_fetch_' . getmypid();
    /// $d = dba_open($f, 'n', 'ndbm'); dba_insert('k', "a\0b", $d); dba_insert('', '', $d);
    /// var_dump(bin2hex(dba_fetch('k', $d)), dba_fetch('', $d), dba_fetch('x', $d), dba_exists('k', $d), dba_exists('x', $d));
    /// var_dump(bin2hex(dba_fetch('k', $d, 1)));
    /// dba_close($d);
    /// $r = dba_open($f, 'r', 'ndbm'); var_dump(bin2hex(dba_fetch('k', $r))); dba_close($r);
    /// unlink($f); unlink("$f.db");
    /// ```
    /// ```expect
    /// string(6) "610062"
    /// string(0) ""
    /// bool(false)
    /// bool(true)
    /// bool(false)
    ///
    /// Notice: dba_fetch(): Handler ndbm does not support optional skip parameter, the value will be ignored in %s on line 5
    /// string(6) "610062"
    /// string(6) "610062"
    /// ```
    pub(crate) fn fetch(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        // SAFETY: `db` is open, and the key's bytes outlive the call,
        // which only reads them.
        let answer = unsafe { dbm_fetch(self.db.as_ptr(), datum(key)) };
        copied(&answer)
    }

    /// `dbm_store` under `DBM_INSERT` or `DBM_REPLACE`: whether it
    /// answered 0, so an insert under a key there is `false`, with no
    /// warning.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/dba_ndbm_store_' . getmypid();
    /// $d = dba_open($f, 'c', 'ndbm');
    /// var_dump(dba_insert('a', '1', $d), dba_insert('a', '2', $d), dba_fetch('a', $d));
    /// var_dump(dba_replace('a', '3', $d), dba_replace('b', '4', $d), dba_fetch('a', $d), dba_fetch('b', $d));
    /// dba_close($d);
    /// $r = dba_open($f, 'r', 'ndbm'); var_dump(dba_insert('c', '5', $r)); dba_close($r);
    /// unlink($f); unlink("$f.db");
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// string(1) "1"
    /// bool(true)
    /// bool(true)
    /// string(1) "3"
    /// string(1) "4"
    ///
    /// Warning: dba_insert(): Cannot perform a modification on a readonly database in %s on line 7
    /// bool(false)
    /// ```
    pub(crate) fn store(&mut self, key: &[u8], value: &[u8], insert: bool) -> bool {
        let flags = if insert { DBM_INSERT } else { DBM_REPLACE };
        // SAFETY: `db` is open, and both datums' bytes outlive the call,
        // which only reads them.
        let answer = unsafe { dbm_store(self.db.as_ptr(), datum(key), datum(value), flags) };
        answer == 0
    }

    /// `dbm_delete`: whether it answered other than -1; a key not there
    /// answers 1.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/dba_ndbm_delete_' . getmypid();
    /// $d = dba_open($f, 'n', 'ndbm'); dba_insert('a', '1', $d);
    /// var_dump(dba_delete('a', $d), dba_delete('a', $d), dba_fetch('a', $d));
    /// dba_close($d); unlink($f); unlink("$f.db");
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ```
    pub(crate) fn delete(&mut self, key: &[u8]) -> bool {
        // SAFETY: `db` is open, and the key's bytes outlive the call,
        // which only reads them.
        let answer = unsafe { dbm_delete(self.db.as_ptr(), datum(key)) };
        answer != -1
    }

    /// `dbm_firstkey`: the walk restarted, in the library's hash order.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/dba_ndbm_walk_' . getmypid();
    /// $d = dba_open($f, 'n', 'ndbm');
    /// var_dump(dba_firstkey($d), dba_nextkey($d));
    /// foreach (['key1', 'key2', '[g]n', 'key number 6'] as $k) dba_insert($k, "v$k", $d);
    /// $keys = [];
    /// for ($k = dba_firstkey($d); $k !== false; $k = dba_nextkey($d)) $keys[] = $k;
    /// var_dump(implode(',', $keys), dba_firstkey($d) === $keys[0]);
    /// dba_close($d); unlink($f); unlink("$f.db");
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(27) "key1,key number 6,key2,[g]n"
    /// bool(true)
    /// ```
    pub(crate) fn first_key(&mut self) -> Option<Vec<u8>> {
        // SAFETY: `db` is open.
        let answer = unsafe { dbm_firstkey(self.db.as_ptr()) };
        copied(&answer)
    }

    /// `dbm_nextkey`: the walk's next key, `false` past the last.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/dba_ndbm_next_' . getmypid();
    /// $d = dba_open($f, 'n', 'ndbm'); dba_insert('only', '1', $d);
    /// var_dump(dba_firstkey($d), dba_nextkey($d), dba_nextkey($d));
    /// dba_close($d); unlink($f); unlink("$f.db");
    /// ```
    /// ```expect
    /// string(4) "only"
    /// bool(false)
    /// bool(false)
    /// ```
    pub(crate) fn next_key(&mut self) -> Option<Vec<u8>> {
        // SAFETY: `db` is open.
        let answer = unsafe { dbm_nextkey(self.db.as_ptr()) };
        copied(&answer)
    }
}

impl Drop for Ndbm {
    /// `dbm_close`, which writes what the library holds.
    fn drop(&mut self) {
        // SAFETY: `db` is open, and this is the last use of it.
        unsafe { dbm_close(self.db.as_ptr()) };
    }
}
