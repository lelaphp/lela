//! `MYSQLND_ERROR_INFO` and `MYSQLND_UPSERT_STATUS`: the last error of a
//! connection or a statement, with the list of every client error set
//! since the last reset that mysqli's `$error_list` reads, and what the
//! last statement changed.

use crate::consts::{ERRMSG_SIZE, UNKNOWN_SQLSTATE};

/// `mysqlnd_server_gone`.
pub(crate) const SERVER_GONE: &str = "MySQL server has gone away";
/// `mysqlnd_out_of_sync`.
pub(crate) const OUT_OF_SYNC: &str = "Commands out of sync; you can't run this command now";
/// `mysqlnd_old_passwd`.
pub(crate) const OLD_PASSWD: &str = "mysqlnd cannot connect to MySQL 4.1+ using the old insecure authentication. Please use an administration tool to reset your password with the command SET PASSWORD = PASSWORD('your_existing_password'). This will store a new, and more secure, hash value in mysql.user. If this user is used in other scripts executed by PHP 5.2 or earlier you might need to remove the old-passwords flag from your my.cnf file";

/// One entry of `error_list`, `MYSQLND_ERROR_LIST_ELEMENT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedError {
    pub errno: u32,
    pub sqlstate: [u8; 5],
    pub error: Vec<u8>,
}

/// `MYSQLND_ERROR_INFO`: the number, the SQLSTATE (empty after a reset,
/// as php's zeroed buffer is), the text cut where php's 512-byte buffer
/// cuts it, and the list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorInfo {
    pub errno: u32,
    pub sqlstate: Option<[u8; 5]>,
    pub error: Vec<u8>,
    pub list: Vec<ListedError>,
}

impl ErrorInfo {
    /// `mysqlnd_error_info::reset`, `SET_EMPTY_ERROR`.
    pub fn reset(&mut self) {
        self.errno = 0;
        self.sqlstate = None;
        self.error.clear();
        self.list.clear();
    }

    /// `SET_CLIENT_ERROR`: a number of zero is a reset.
    pub fn set(&mut self, errno: u32, sqlstate: &[u8], error: &[u8]) {
        if errno == 0 {
            self.reset();
            return;
        }
        let state = state_of(sqlstate);
        let error = cut(error);
        self.errno = errno;
        self.sqlstate = (state[0] != 0).then_some(state);
        self.error.clone_from(&error);
        self.list.push(ListedError {
            errno,
            sqlstate: state,
            error,
        });
    }

    /// What `php_mysqlnd_read_error_from_line` writes straight into the
    /// fields, past `set_client_error`, so the list is not added to.
    pub fn raw(&mut self, errno: u32, sqlstate: [u8; 5], error: &[u8]) {
        self.errno = errno;
        self.sqlstate = Some(sqlstate);
        self.error = cut(error);
    }

    /// `SET_CLIENT_ERROR` with `UNKNOWN_SQLSTATE` and a text of mysqlnd's.
    pub fn client(&mut self, errno: u32, error: &str) {
        self.set(errno, UNKNOWN_SQLSTATE, error.as_bytes());
    }

    /// `COPY_CLIENT_ERROR`: another error info's number, state and text.
    pub fn copy_from(&mut self, from: &ErrorInfo) {
        let state = from.sqlstate.unwrap_or([0; 5]);
        self.set(from.errno, &state, &from.error.clone());
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
    let end = error.iter().position(|b| *b == 0).unwrap_or(error.len());
    error[..end.min(ERRMSG_SIZE - 1)].to_vec()
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
