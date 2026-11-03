//! The `bz2` extension: `php-src/ext/bz2` as one crate under `ext/`
//! (ADR-0004), over `libbz2-rs-sys` bound in [`ffi`] alone (design §7.2,
//! revision 39). Its state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/bz2/tests`, through `xtask phpt --subset`.
//!
//! Declared by the attributes as `bz2.stub.php` at the tag declares it
//! and `php -n --re bz2` renders it: the one-shot `bzcompress` and
//! `bzdecompress` ([`buffer`]), the `compress.bzip2://` wrapper's stream
//! with `bzopen`, `bzread` and the error natives ([`stream`]), and the
//! `bzip2.*` stream filters ([`filter`]). phar's bzip2 compression rides
//! on the filters, as php's does.
//!
//! The library is libbzip2 ported to Rust and not the platform's libbz2:
//! php's tests observe the compressed bytes, and on #1116's measurement
//! over nine inputs, every block size 1 to 9 and every work factor 0 to
//! 250 (20,331 rows), `libbz2-rs-sys` 0.2.5 wrote the oracle's bytes on
//! every row, as the platform's libbz2 1.0.8 does, where `oxiarc-bzip2`
//! and `crabz2` wrote them for the empty input alone.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod buffer;
mod ffi;
mod filter;
mod stream;

use std::collections::HashMap;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::raising::Thrown;
use php_ext::request::{Request, Streams as _};

use crate::ffi::Code;
use crate::stream::FileId;

/// `BZ2_bzlibVersion()` of the libbzip2 whose bytes the library writes, the
/// oracle's 1.0.8, as ext/iconv and ext/lexbor name the library they
/// reproduce; `libbz2-rs-sys` names itself `1.1.0-libbz2-rs-sys-0.2.5`.
const LIBRARY_VERSION: &str = "1.0.8, 13-Jul-2019";

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. The open `BZip2` streams' files: the next number, each
/// file's `lastErr` by its number, and the number of the stream each
/// resource is, which `php_stream_is(stream, PHP_STREAM_IS_BZIP2)` asks.
#[derive(Default)]
struct State {
    next: u64,
    errors: HashMap<FileId, Code>,
    streams: HashMap<i64, FileId>,
}

/// The extension value (design §7.1): the natives in `bz2.stub.php`'s
/// order, the wrapper and the filters `MINIT` registers, and the state
/// declared on it.
///
/// ```php
/// var_dump(extension_loaded('bz2'), in_array('compress.bzip2', stream_get_wrappers()), in_array('bzip2.*', stream_get_filters()));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(&[
            stream::bzopen::DECLARED,
            stream::bzread::DECLARED,
            stream::bzwrite::DECLARED,
            stream::bzflush::DECLARED,
            stream::bzclose::DECLARED,
            stream::bzerrno::DECLARED,
            stream::bzerrstr::DECLARED,
            stream::bzerror::DECLARED,
            buffer::bzcompress::DECLARED,
            buffer::bzdecompress::DECLARED,
        ])
        .request_state::<State>()
        .rinit(rinit)
        .info(info)
        .build("bz2")
}

/// `PHP_MINIT_FUNCTION(bz2)`'s registrations, made per request as the
/// engine keeps its wrapper and filter tables.
#[allow(clippy::unnecessary_wraps)] // the ABI's `rinit` hook
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.register_wrapper(stream::wrapper());
    req.register_filter(filter::NAME, filter::factory);
    Ok(())
}

/// `PHP_MINFO_FUNCTION(bz2)`.
///
/// ```php
/// (new ReflectionExtension('bz2'))->info();
/// ```
/// ```expect
///
/// bz2
///
/// BZip2 Support => Enabled
/// Stream Wrapper support => compress.bzip2://
/// Stream Filter support => bzip2.decompress, bzip2.compress
/// BZip2 Version => 1.0.8, 13-Jul-2019
/// ```
fn info(req: &mut Request) {
    req.extensions_mut().info_table(&[
        &["BZip2 Support", "Enabled"],
        &["Stream Wrapper support", "compress.bzip2://"],
        &["Stream Filter support", "bzip2.decompress, bzip2.compress"],
        &["BZip2 Version", LIBRARY_VERSION],
    ]);
}
