//! The protocol codecs (design §4.8): the `FastCGI` record wire format,
//! CGI's environment and response head, the dev server's HTTP/1.x
//! framing, and the built-in server's path rules, which `php -S` and the
//! production server find a request's file by (`vpath`, #2588). Each
//! takes bytes to plain values and plain values back to bytes, but for
//! the two that ask the file system, through [`cgi::Files`], as php asks
//! `stat(2)`: `cgi::request_info` and `vpath::translate`. `php_sapi`
//! turns what they decode into a request description.
//! Nothing here names the engine or the heap, so a protocol test builds
//! this crate alone.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

pub mod cgi;
pub mod fcgi;
pub mod http;
pub mod mime;
pub mod vpath;

/// `bytes` as php reads a `char *` through `strlen`: the bytes before the
/// first NUL. A file system call reads a path so, and so does every value
/// php registers without its length.
#[must_use]
pub fn c_str(bytes: &[u8]) -> &[u8] {
    bytes.split(|&b| b == 0).next().unwrap_or(bytes)
}
