//! `MYSQLND_ERROR_INFO` and `MYSQLND_UPSERT_STATUS`: the last error of a
//! connection or a statement, with the list of every client error set
//! since the last reset that mysqli's `$error_list` reads, and what the
//! last statement changed.

use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::alloc::Alloc;
use crate::consts::{ERRMSG_SIZE, UNKNOWN_SQLSTATE};

/// `mysqlnd_server_gone`.
pub(crate) const SERVER_GONE: &str = "MySQL server has gone away";
/// `mysqlnd_out_of_memory`.
pub(crate) const OUT_OF_MEMORY: &str = "Out of memory";
/// `mysqlnd_out_of_sync`.
pub(crate) const OUT_OF_SYNC: &str = "Commands out of sync; you can't run this command now";
/// `mysqlnd_old_passwd`.
pub(crate) const OLD_PASSWD: &str = "mysqlnd cannot connect to MySQL 4.1+ using the old insecure authentication. Please use an administration tool to reset your password with the command SET PASSWORD = PASSWORD('your_existing_password'). This will store a new, and more secure, hash value in mysql.user. If this user is used in other scripts executed by PHP 5.2 or earlier you might need to remove the old-passwords flag from your my.cnf file";

/// `sizeof(zend_llist_element) + sizeof(MYSQLND_ERROR_LIST_ELEMENT) - 1`:
/// what `zend_llist_add_element` asks for an entry of the list.
const LIST_ELEMENT: usize = 24 + 24 - 1;

/// One entry of `error_list`, `MYSQLND_ERROR_LIST_ELEMENT`: the
/// number, the state, and the text `mnd_pestrdup(error, TRUE)` copies
/// with `malloc`, which no request counts; and the list's element that
/// holds them, `pemalloc`'d as the list is persistent.
#[derive(Debug)]
pub struct ListedError {
    pub errno: u32,
    pub sqlstate: [u8; 5],
    pub error: Vec<u8>,
    held: Alloc,
}

/// `MYSQLND_ERROR_INFO`: the number, the SQLSTATE (empty after a reset,
/// as php's zeroed buffer is), the text cut where php's 512-byte buffer
/// cuts it, and the list, whose elements are the request's unless the
/// error info is a persistent connection's.
#[derive(Debug, Default)]
pub struct ErrorInfo {
    pub errno: u32,
    pub sqlstate: Option<[u8; 5]>,
    pub error: Vec<u8>,
    list: Vec<ListedError>,
    persistent: bool,
}

impl ErrorInfo {
    /// `mysqlnd_error_info_init(info, persistent)`.
    pub fn new(persistent: bool) -> ErrorInfo {
        ErrorInfo {
            persistent,
            ..ErrorInfo::default()
        }
    }

    /// `mysqlnd_error_info::reset`, `SET_EMPTY_ERROR`, and
    /// `mysqlnd_error_info_free_contents`: the list's elements freed by
    /// `zend_llist_clean`.
    pub fn reset(&mut self, req: &mut Request) {
        self.errno = 0;
        self.sqlstate = None;
        self.error.clear();
        self.clean(req);
    }

    /// `zend_llist_clean(&info->error_list)`: the list alone emptied.
    pub fn clean(&mut self, req: &mut Request) {
        for listed in self.list.drain(..) {
            listed.held.free(req);
        }
    }

    /// Every client error set since the last reset, mysqli's
    /// `$error_list`.
    pub fn list(&self) -> &[ListedError] {
        &self.list
    }

    /// `SET_CLIENT_ERROR`: a number of zero is a reset; the `Err` is the
    /// memory limit's fatal, met by the list's element. Each error set
    /// adds its 47-byte element to the request's heap, which the next
    /// reset frees, and a list mysqli keeps across a call it frees what
    /// the call added:
    ///
    /// ```php
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// mysqli_query($l, 'SELEC 1'); mysqli_query($l, 'DO 1');
    /// $m = memory_get_usage();
    /// mysqli_query($l, 'SELEC 1');
    /// echo memory_get_usage() - $m, "\n";
    /// mysqli_multi_query($l, 'SELEC 1; DO 2');
    /// echo memory_get_usage() - $m, "\n";
    /// mysqli_query($l, 'DO 1');
    /// echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect mysql
    /// 48
    /// 80
    /// 32
    /// ```
    pub fn set(
        &mut self,
        req: &mut Request,
        errno: u32,
        sqlstate: &[u8],
        error: &[u8],
    ) -> Result<(), Thrown> {
        if errno == 0 {
            self.reset(req);
            return Ok(());
        }
        let state = state_of(sqlstate);
        let error = cut(error);
        self.errno = errno;
        self.sqlstate = (state[0] != 0).then_some(state);
        self.error.clone_from(&error);
        let held = Alloc::new(req, LIST_ELEMENT, self.persistent)?;
        self.list.push(ListedError {
            errno,
            sqlstate: state,
            error,
            held,
        });
        Ok(())
    }

    /// What `php_mysqlnd_read_error_from_line` writes straight into the
    /// fields, past `set_client_error`, so the list is not added to.
    pub fn raw(&mut self, errno: u32, sqlstate: [u8; 5], error: &[u8]) {
        self.errno = errno;
        self.sqlstate = Some(sqlstate);
        self.error = cut(error);
    }

    /// `SET_CLIENT_ERROR` with `UNKNOWN_SQLSTATE` and a text of mysqlnd's.
    pub fn client(&mut self, req: &mut Request, errno: u32, error: &str) -> Result<(), Thrown> {
        self.set(req, errno, UNKNOWN_SQLSTATE, error.as_bytes())
    }

    /// `COPY_CLIENT_ERROR`: another error info's number, state and text.
    pub fn copy_from(&mut self, req: &mut Request, from: &ErrorInfo) -> Result<(), Thrown> {
        let state = from.sqlstate.unwrap_or([0; 5]);
        self.set(req, from.errno, &state, &from.error)
    }

    /// The error info copied by value with its list moved out, the list
    /// here left empty: mysqli's `MYSQLND_ERROR_INFO error_info =
    /// *info` with `error_list`'s head, tail and count cleared, which
    /// [`ErrorInfo::restore`] puts back.
    pub fn keep(&mut self) -> ErrorInfo {
        ErrorInfo {
            errno: self.errno,
            sqlstate: self.sqlstate,
            error: self.error.clone(),
            list: std::mem::take(&mut self.list),
            persistent: self.persistent,
        }
    }

    /// `zend_llist_clean(&info->error_list)` and `*info = kept`: what
    /// the list grew since [`ErrorInfo::keep`] freed, the kept error
    /// put back.
    pub fn restore(&mut self, req: &mut Request, kept: ErrorInfo) {
        self.clean(req);
        *self = kept;
    }

    /// The SQLSTATE as `mysqlnd_conn_data::sqlstate` answers it: `00000`
    /// where none is set.
    pub fn state(&self) -> [u8; 5] {
        self.sqlstate.unwrap_or(*b"00000")
    }
}

/// The five bytes of an SQLSTATE php copies with `strlcpy` into a
/// six-byte buffer.
fn state_of(sqlstate: &[u8]) -> [u8; 5] {
    let mut state = [0u8; 5];
    for (to, from) in state
        .iter_mut()
        .zip(sqlstate.iter().take_while(|b| **b != 0))
    {
        *to = *from;
    }
    state
}

/// The text as `strlcpy` into `MYSQLND_ERRMSG_SIZE` keeps it: up to the
/// first NUL, at most 511 bytes.
fn cut(error: &[u8]) -> Vec<u8> {
    let error = php_ext::coerce::c_str(error);
    error[..error.len().min(ERRMSG_SIZE - 1)].to_vec()
}

/// `MYSQLND_UPSERT_STATUS`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Upsert {
    pub warning_count: u32,
    pub server_status: u32,
    pub affected_rows: u64,
    pub last_insert_id: u64,
}

impl Upsert {
    /// `UPSERT_STATUS_RESET`.
    pub fn reset(&mut self) {
        *self = Upsert::default();
    }

    /// `UPSERT_STATUS_SET_AFFECTED_ROWS_TO_ERROR`: `(uint64_t) ~0`.
    pub fn affected_rows_to_error(&mut self) {
        self.affected_rows = u64::MAX;
    }
}
