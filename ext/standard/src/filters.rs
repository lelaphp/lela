//! `Z/ext/standard/filters.c`'s string filters, `string.rot13`,
//! `string.toupper` and `string.tolower`, each a [`StreamFilter`] that
//! maps every bucket it is handed byte for byte and passes it on, and
//! the registration `PHP_MINIT(standard_filters)` makes of them, served
//! here at request start since the request's filter table is the one a
//! program's `stream_filter_register` adds to. The `convert.*` family,
//! `consumed` and `dechunk` are not here yet.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::raising::Thrown;
use php_ext::request::{Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter};
use php_ext::values::Value;

/// One of the three string filters: the byte map it applies.
struct Strfilter {
    map: fn(u8) -> u8,
}

/// `rot13_from` to `rot13_to` (`Z/ext/standard/filters.c:28`).
const fn rot13(b: u8) -> u8 {
    match b {
        b'a'..=b'm' | b'A'..=b'M' => b + 13,
        b'n'..=b'z' | b'N'..=b'Z' => b - 13,
        _ => b,
    }
}

impl StreamFilter for Strfilter {
    /// `strfilter_rot13_filter` and its two siblings
    /// (`Z/ext/standard/filters.c:32`): every bucket mapped, counted as
    /// consumed and handed on, and `PSFS_PASS_ON`.
    fn filter(
        &mut self,
        _req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        _flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let mut count = 0;
        while let Some(mut bucket) = input.pop_front() {
            for b in &mut bucket {
                *b = (self.map)(*b);
            }
            count += bucket.len();
            output.push_back(bucket);
        }
        if let Some(n) = consumed {
            *n = count;
        }
        Ok(FilterStatus::PassOn)
    }
}

/// The three factories, `strfilter_rot13_create` and its siblings.
#[allow(clippy::unnecessary_wraps)] // `FilterFactory`'s signature
fn rot13_factory(
    _req: &mut Request,
    _name: &[u8],
    _params: Option<&Value>,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    Ok(Some(Box::new(Strfilter { map: rot13 })))
}

#[allow(clippy::unnecessary_wraps)] // `FilterFactory`'s signature
fn toupper_factory(
    _req: &mut Request,
    _name: &[u8],
    _params: Option<&Value>,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    Ok(Some(Box::new(Strfilter {
        map: |b| b.to_ascii_uppercase(),
    })))
}

#[allow(clippy::unnecessary_wraps)] // `FilterFactory`'s signature
fn tolower_factory(
    _req: &mut Request,
    _name: &[u8],
    _params: Option<&Value>,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    Ok(Some(Box::new(Strfilter {
        map: |b| b.to_ascii_lowercase(),
    })))
}

/// `PHP_MINIT(standard_filters)` (`Z/ext/standard/filters.c:1904`)'s
/// three string filters registered in its order, at request start.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "Hello, World!\n"); rewind($m);
/// stream_filter_append($m, 'string.rot13');
/// var_dump(fgets($m));
/// foreach (['string.toupper', 'string.tolower'] as $f) {
///     $w = fopen('php://memory', 'w+'); stream_filter_append($w, $f, STREAM_FILTER_WRITE);
///     fwrite($w, "MiXeD 123"); rewind($w); var_dump(fread($w, 20));
/// }
/// ```
/// ```expect
/// string(14) "Uryyb, Jbeyq!
/// "
/// string(9) "MIXED 123"
/// string(9) "mixed 123"
/// ```
pub(crate) fn register(req: &mut Request) {
    req.register_filter(b"string.rot13", rot13_factory);
    req.register_filter(b"string.toupper", toupper_factory);
    req.register_filter(b"string.tolower", tolower_factory);
}
