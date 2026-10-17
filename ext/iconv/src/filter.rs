//! `iconv.c`'s stream filter, `php_iconv_stream_filter_*`
//! (`Z/ext/iconv/iconv.c:2325-2660`): the `convert.iconv.*` factory,
//! which reads the two charsets off the name the program asked for, and
//! the filter that converts each bucket through one converter held for
//! as long as the filter is on its chain, keeping a character a bucket
//! ends inside of for the next.
//
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use crate::MAX_NAME;
use crate::ffi::{Cd, Errno};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter};
use php_ext::values::Value;

/// The name php registers the factory under.
pub(crate) const NAME: &[u8] = b"convert.iconv.*";

/// `sizeof(self->stub)`: the most bytes of an unfinished character
/// the filter carries to the next bucket.
const STUB: usize = 128;

/// `php_iconv_stream_filter`: the converter, the two names its warnings
/// print, and `stub`, the bytes of a character the last bucket ended
/// inside of.
struct Filter {
    cd: Cd,
    from: String,
    to: String,
    stub: Vec<u8>,
}

/// Why a bucket failed, each one php's warning.
#[derive(Clone, Copy)]
enum Failure {
    InvalidSequence,
    InsufficientBuffer,
    UnexpectedOctets,
    Unknown,
}

/// The bytes one call makes: `out` with the part of it written, doubled
/// at each `E2BIG` as `perealloc` doubles it.
struct Room {
    out: Vec<u8>,
    used: usize,
}

impl Room {
    /// One `iconv` call over `input` into the room left, `None` the
    /// flush of the shift state.
    fn step(&mut self, cd: &mut Cd, input: Option<&mut &[u8]>) -> Result<usize, Errno> {
        let (written, result) = cd.step(input, &mut self.out[self.used..]);
        self.used += written;
        result
    }

    fn grow(&mut self) {
        let grown = self.out.len().max(1) << 1;
        self.out.resize(grown, 0);
    }
}

impl Filter {
    /// `php_iconv_stream_filter_append_bucket`: the carried bytes first,
    /// each byte of `bucket` they still want moved onto them, a bucket
    /// that ends before they make a character dropping them as php's
    /// `tcnt = 0` does; then `bucket`, or with `None` the shift state,
    /// its unfinished tail carried. What converted is one bucket on
    /// `output`.
    fn append(&mut self, bucket: Option<&[u8]>, output: &mut Brigade) -> Result<(), Failure> {
        let mut rest = bucket.unwrap_or_default();
        let initial = bucket.map_or(64, <[u8]>::len);
        let mut room = Room {
            out: vec![0; initial],
            used: 0,
        };
        if !self.stub.is_empty() {
            let mut at = 0;
            let mut dropped = false;
            while at < self.stub.len() {
                let mut pt = &self.stub[at..];
                let result = room.step(&mut self.cd, Some(&mut pt));
                at = self.stub.len() - pt.len();
                match result {
                    Ok(_) => {}
                    Err(Errno::TooBig) => room.grow(),
                    Err(Errno::Ilseq) => return Err(Failure::InvalidSequence),
                    Err(Errno::Inval) if bucket.is_none() => return Err(Failure::InvalidSequence),
                    Err(Errno::Inval) => {
                        let Some((&byte, tail)) = rest.split_first() else {
                            dropped = true;
                            break;
                        };
                        if self.stub.len() >= STUB {
                            return Err(Failure::InsufficientBuffer);
                        }
                        self.stub.push(byte);
                        rest = tail;
                        at = 0;
                    }
                    Err(Errno::Other(_)) => return Err(Failure::Unknown),
                }
            }
            if dropped {
                self.stub.clear();
            } else {
                self.stub.drain(..at);
            }
        }
        loop {
            let result = match bucket {
                Some(_) if rest.is_empty() => break,
                Some(_) => room.step(&mut self.cd, Some(&mut rest)),
                None => room.step(&mut self.cd, None),
            };
            match result {
                Ok(_) if bucket.is_none() => break,
                Ok(_) => {}
                Err(Errno::TooBig) => room.grow(),
                Err(Errno::Ilseq) => return Err(Failure::InvalidSequence),
                Err(Errno::Inval) if bucket.is_none() => return Err(Failure::UnexpectedOctets),
                Err(Errno::Inval) => {
                    if rest.len() > STUB {
                        return Err(Failure::InsufficientBuffer);
                    }
                    self.stub = rest.to_vec();
                    rest = &[];
                }
                Err(Errno::Other(_)) => return Err(Failure::Unknown),
            }
        }
        if room.used > 0 {
            room.out.truncate(room.used);
            output.push_back(room.out);
        }
        Ok(())
    }

    /// php's warning for a bucket that failed.
    fn warn(&self, req: &mut Request, failure: Failure) {
        let what = match failure {
            Failure::InvalidSequence => "invalid multibyte sequence",
            Failure::InsufficientBuffer => "insufficient buffer",
            Failure::UnexpectedOctets => "unexpected octet values",
            Failure::Unknown => "unknown error",
        };
        let text = format!(
            "iconv stream filter (\"{}\"=>\"{}\"): {what}",
            self.from, self.to
        );
        docref(req, Level::Warning, &text);
    }
}

impl StreamFilter for Filter {
    /// `php_iconv_stream_filter_do_filter`: each bucket of `input`
    /// converted onto `output`, then, on a flush, the carried bytes and
    /// the shift state; `PSFS_ERR_FATAL` after the warning of the first
    /// that failed.
    ///
    /// ```php
    /// $m = fopen('php://memory', 'w+'); fwrite($m, "\x1b\$B\$3\$s\x1b(B!"); rewind($m);
    /// stream_filter_append($m, 'convert.iconv.ISO-2022-JP/UTF-8');
    /// var_dump(stream_get_contents($m));
    /// $w = fopen('php://memory', 'w+'); stream_filter_append($w, 'convert.iconv.UTF-8.UTF-16BE', STREAM_FILTER_WRITE);
    /// var_dump(fwrite($w, "a\xc3"), fwrite($w, "\xa9b"), fwrite($w, "\xe2"), fwrite($w, "\x82"), fwrite($w, "\xacc"));
    /// rewind($w); var_dump(bin2hex(stream_get_contents($w)));
    /// ```
    /// ```expect
    /// string(7) "こん!"
    ///
    /// Warning: fwrite(): iconv stream filter ("UTF-8"=>"UTF-16BE"): invalid multibyte sequence in %s on line 6
    /// int(2)
    /// int(2)
    /// int(1)
    /// int(1)
    /// bool(false)
    /// string(12) "006100e90062"
    /// ```
    ///
    /// A character begun in one bucket and not finished in the next is
    /// php's warning and the stream's end.
    ///
    /// ```php
    /// $w = fopen('php://memory', 'w+'); stream_filter_append($w, 'convert.iconv.UTF-8/ISO-8859-1', STREAM_FILTER_WRITE);
    /// var_dump(fwrite($w, "ok\xc3"), fwrite($w, "x"), fwrite($w, "more"));
    /// rewind($w); var_dump(stream_get_contents($w)); fclose($w);
    /// $r = fopen('php://memory', 'w+'); fwrite($r, "ab\xe2\x82"); rewind($r);
    /// stream_filter_append($r, 'convert.iconv.UTF-8/UTF-16LE');
    /// var_dump(stream_get_contents($r));
    /// ```
    /// ```expect
    ///
    /// Warning: fwrite(): iconv stream filter ("UTF-8"=>"ISO-8859-1"): invalid multibyte sequence in %s on line 3
    ///
    /// Warning: fwrite(): iconv stream filter ("UTF-8"=>"ISO-8859-1"): invalid multibyte sequence in %s on line 3
    /// int(3)
    /// bool(false)
    /// bool(false)
    ///
    /// Warning: rewind(): iconv stream filter ("UTF-8"=>"ISO-8859-1"): invalid multibyte sequence in %s on line 4
    /// string(2) "ok"
    ///
    /// Warning: fclose(): iconv stream filter ("UTF-8"=>"ISO-8859-1"): invalid multibyte sequence in %s on line 4
    ///
    /// Warning: stream_get_contents(): iconv stream filter ("UTF-8"=>"UTF-16LE"): invalid multibyte sequence in %s on line 7
    /// string(0) ""
    /// ```
    fn filter(
        &mut self,
        req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let mut count = 0;
        while let Some(bucket) = input.pop_front() {
            if let Err(failure) = self.append(Some(&bucket), output) {
                self.warn(req, failure);
                return Ok(FilterStatus::ErrFatal);
            }
            count += bucket.len();
        }
        if flags != FilterFlags::Normal
            && let Err(failure) = self.append(None, output)
        {
            self.warn(req, failure);
            return Ok(FilterStatus::ErrFatal);
        }
        if let Some(n) = consumed {
            *n = count;
        }
        Ok(FilterStatus::PassOn)
    }
}

/// `php_iconv_stream_filter_factory_create`: the name's third segment
/// on is `FROM/TO` or `FROM.TO`, read as C strings read, up to a NUL; a
/// name without both, either charset of 64 bytes or more, or a pair the
/// host cannot convert is a refusal, which the chain's lookup warns of.
///
/// ```php
/// var_dump(in_array('convert.iconv.*', stream_get_filters()));
/// $m = fopen('php://memory', 'w+'); fwrite($m, 'x'); rewind($m);
/// var_dump(stream_filter_append($m, 'convert.iconv.UTF-8'), stream_filter_append($m, 'convert.iconv.UTF-8/nope'), stream_filter_append($m, 'convert.iconv.' . str_repeat('a', 64) . '/UTF-8'));
/// var_dump(is_resource(stream_filter_append($m, 'convert.iconv.utf-8.ascii')), fread($m, 9));
/// ```
/// ```expect
/// bool(true)
///
/// Warning: stream_filter_append(): Unable to create or locate filter "convert.iconv.UTF-8" in %s on line 4
///
/// Warning: stream_filter_append(): Unable to create or locate filter "convert.iconv.UTF-8/nope" in %s on line 4
///
/// Warning: stream_filter_append(): Unable to create or locate filter "convert.iconv.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/UTF-8" in %s on line 4
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// string(1) "x"
/// ```
#[allow(clippy::unnecessary_wraps)] // `FilterFactory`'s signature
pub(crate) fn factory(
    _req: &mut Request,
    name: &[u8],
    _params: Option<&Value>,
    _persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    Ok(charsets(name).and_then(|(from, to)| {
        let cd = Cd::open(to, from).ok()?;
        let named = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        let filter = Filter {
            cd,
            from: named(from),
            to: named(to),
            stub: Vec::new(),
        };
        Some(Box::new(filter) as Box<dyn StreamFilter>)
    }))
}

/// The `from` and `to` of a filter's name, php's `strchr` twice and
/// `strpbrk(from, "/.")`.
fn charsets(name: &[u8]) -> Option<(&[u8], &[u8])> {
    fn after(bytes: &[u8]) -> Option<&[u8]> {
        let at = bytes.iter().position(|&b| b == b'.')?;
        Some(&bytes[at + 1..])
    }
    let name = name.split(|&b| b == 0).next().unwrap_or_default();
    let from = after(after(name)?)?;
    let cut = from.iter().position(|&b| b == b'/' || b == b'.')?;
    let (from, to) = (&from[..cut], &from[cut + 1..]);
    (from.len() < MAX_NAME && to.len() < MAX_NAME).then_some((from, to))
}
