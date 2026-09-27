//! `zlib.c`'s one-shot natives: `PHP_ZLIB_ENCODE_FUNC`'s three and
//! `zlib_encode`, over `php_zlib_encode`, and `PHP_ZLIB_DECODE_FUNC`'s
//! three and `zlib_decode`, over `php_zlib_decode` and
//! `php_zlib_inflate_rounds`, each copied with its buffer sizes, since
//! the rounds decide when a `$max_length` is exceeded.

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::PhpString;

use crate::constants::{ZLIB_ENCODING_DEFLATE, ZLIB_ENCODING_GZIP, ZLIB_ENCODING_RAW};
use crate::ffi::{Deflate, Flush, Inflate, Status};

/// `PHP_ZLIB_ENCODING_ANY`: `inflateInit2`'s automatic gzip-or-zlib
/// header detection, what `zlib_decode` reads with first.
const ENCODING_ANY: i64 = 0x2f;

/// `MAX_MEM_LEVEL`, the memory level php's one-shot encoders and its
/// buffer handler deflate with.
pub(crate) const MAX_MEM_LEVEL: i64 = 9;

/// `Z_DEFAULT_STRATEGY`.
pub(crate) const DEFAULT_STRATEGY: i64 = 0;

/// `PHP_ZLIB_BUFFER_SIZE_GUESS(in_len)`: the input's length and 1.5%,
/// and 23 bytes of headers and trailers.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)] // php's own `(size_t) ((double) in_len * (double) 1.015)`
pub(crate) fn size_guess(len: usize) -> usize {
    ((len as f64) * 1.015) as usize + 10 + 8 + 4 + 1
}

/// `php_zlib_encode`: one `deflate(Z_FINISH)` into a buffer of
/// [`size_guess`], and php's warning with zlib's text where the init or
/// the deflate did not end the stream.
fn encode(
    req: &mut Request,
    input: &[u8],
    encoding: i64,
    level: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    let status = match Deflate::new(level, encoding, MAX_MEM_LEVEL, DEFAULT_STRATEGY) {
        Ok(mut stream) => {
            let mut out = vec![0; size_guess(input.len())];
            let step = stream.deflate(input, &mut out, Flush::Finish);
            if step.status == Status::StreamEnd {
                out.truncate(step.written);
                return Ok(OrFalse::Value(PhpString::new(req, &out)?));
            }
            step.status
        }
        Err(status) => status,
    };
    docref(req, Level::Warning, &status.message());
    Ok(OrFalse::False)
}

/// `PHP_ZLIB_ENCODE_FUNC`'s checks: the level first, then the encoding,
/// each named by its position in the native at hand.
fn checked(
    req: &mut Request,
    level: i64,
    level_at: u32,
    encoding: i64,
    encoding_at: u32,
) -> Result<(), Thrown> {
    if !(-1..=9).contains(&level) {
        return Err(arg_value_error(req, level_at, "be between -1 and 9"));
    }
    if ![ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, ZLIB_ENCODING_DEFLATE].contains(&encoding) {
        return Err(arg_value_error(
            req,
            encoding_at,
            "be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE",
        ));
    }
    Ok(())
}

/// `gzcompress(string $data, int $level = -1, int $encoding =
/// ZLIB_ENCODING_DEFLATE): string|false`. The bytes are the linked
/// zlib's, the oracle's at every level.
///
/// ```php
/// $d = str_repeat("The quick brown fox jumps over the lazy dog. 0123456789\n", 40) . implode(',', range(1, 500));
/// foreach (range(-1, 9) as $l) echo $l, ' ', strlen(gzcompress($d, $l)), ' ', md5(gzcompress($d, $l)), "\n";
/// echo bin2hex(gzcompress('hello hello hello')), "\n";
/// echo bin2hex(gzcompress('hello', 9, ZLIB_ENCODING_GZIP)), "\n";
/// try { gzcompress('x', 10); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { gzcompress('x', 1, 99); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// -1 999 dc87a29105188b0ac6acccb6dee9f902
/// 0 4142 5c7619e0032d4c929bf77154b9417d9c
/// 1 984 30fd3da9ae3b23187ed39fc4051d76ac
/// 2 981 791a0aa62bbe5df120c61f32d4ea5387
/// 3 981 791a0aa62bbe5df120c61f32d4ea5387
/// 4 999 fa2ae443866abe608a65852a690de53d
/// 5 999 fa2ae443866abe608a65852a690de53d
/// 6 999 dc87a29105188b0ac6acccb6dee9f902
/// 7 999 68c32458938bd8de12dd58841690c83f
/// 8 999 68c32458938bd8de12dd58841690c83f
/// 9 999 68c32458938bd8de12dd58841690c83f
/// 789ccb48cdc9c957c84090003a2e067d
/// 1f8b0800000000000213cb48cdc9c9070086a6103605000000
/// gzcompress(): Argument #2 ($level) must be between -1 and 9
/// gzcompress(): Argument #3 ($encoding) must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE
/// ```
#[php_function]
fn gzcompress(
    req: &mut Request,
    data: PhpString,
    #[php(default = -1)] level: i64,
    #[php(default = ZLIB_ENCODING_DEFLATE)] encoding: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    checked(req, level, 2, encoding, 3)?;
    encode(req, data.as_bytes(), encoding, level)
}

/// `gzdeflate(string $data, int $level = -1, int $encoding =
/// ZLIB_ENCODING_RAW): string|false`.
///
/// ```php
/// $d = str_repeat("The quick brown fox jumps over the lazy dog. 0123456789\n", 40) . implode(',', range(1, 500));
/// foreach (range(-1, 9) as $l) echo $l, ' ', strlen(gzdeflate($d, $l)), ' ', md5(gzdeflate($d, $l)), "\n";
/// echo bin2hex(gzdeflate('hello hello hello')), "\n", bin2hex(gzdeflate('')), "\n";
/// try { gzdeflate('x', -2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// -1 993 b3d0b49e2d9d885ab5fd31af4f47f349
/// 0 4136 48fcebbc4d222bea45c269451ce93e00
/// 1 978 fdd3d2a128107bbae9ff12580a97a5be
/// 2 975 babd2523440b05223cc06dd22d614e06
/// 3 975 babd2523440b05223cc06dd22d614e06
/// 4 993 b3d0b49e2d9d885ab5fd31af4f47f349
/// 5 993 b3d0b49e2d9d885ab5fd31af4f47f349
/// 6 993 b3d0b49e2d9d885ab5fd31af4f47f349
/// 7 993 b3d0b49e2d9d885ab5fd31af4f47f349
/// 8 993 b3d0b49e2d9d885ab5fd31af4f47f349
/// 9 993 b3d0b49e2d9d885ab5fd31af4f47f349
/// cb48cdc9c957c8409000
/// 0300
/// gzdeflate(): Argument #2 ($level) must be between -1 and 9
/// ```
#[php_function]
fn gzdeflate(
    req: &mut Request,
    data: PhpString,
    #[php(default = -1)] level: i64,
    #[php(default = ZLIB_ENCODING_RAW)] encoding: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    checked(req, level, 2, encoding, 3)?;
    encode(req, data.as_bytes(), encoding, level)
}

/// `gzencode(string $data, int $level = -1, int $encoding =
/// ZLIB_ENCODING_GZIP): string|false`: the gzip header is the linked
/// zlib's, with its operating system byte.
///
/// ```php
/// $d = str_repeat("The quick brown fox jumps over the lazy dog. 0123456789\n", 40) . implode(',', range(1, 500));
/// foreach (range(-1, 9) as $l) echo $l, ' ', strlen(gzencode($d, $l)), ' ', md5(gzencode($d, $l)), "\n";
/// echo bin2hex(gzencode('hello hello hello')), "\n";
/// echo bin2hex(gzencode('hello', 1, FORCE_DEFLATE)), "\n";
/// ```
/// ```expect
/// -1 1011 33e53dd1f641d87772f9f139209f5943
/// 0 4154 17edc77b8b028ca23f4f12546793c83b
/// 1 996 2f27813bb781fd8a8459f14f65344511
/// 2 993 9ae4854f0b1800bea5879c726c90a7c2
/// 3 993 9ae4854f0b1800bea5879c726c90a7c2
/// 4 1011 33e53dd1f641d87772f9f139209f5943
/// 5 1011 33e53dd1f641d87772f9f139209f5943
/// 6 1011 33e53dd1f641d87772f9f139209f5943
/// 7 1011 33e53dd1f641d87772f9f139209f5943
/// 8 1011 33e53dd1f641d87772f9f139209f5943
/// 9 1011 cf1b4d389c3952b441a74c7b53377ccd
/// 1f8b0800000000000013cb48cdc9c957c84090008088f9e511000000
/// 7801cb48cdc9c90700062c0215
/// ```
#[php_function]
fn gzencode(
    req: &mut Request,
    data: PhpString,
    #[php(default = -1)] level: i64,
    #[php(default = ZLIB_ENCODING_GZIP)] encoding: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    checked(req, level, 2, encoding, 3)?;
    encode(req, data.as_bytes(), encoding, level)
}

/// `zlib_encode(string $data, int $encoding, int $level = -1):
/// string|false`, whose level is the third argument.
///
/// ```php
/// echo bin2hex(zlib_encode('abcabcabc', ZLIB_ENCODING_RAW)), "\n";
/// echo bin2hex(zlib_encode('abcabcabc', ZLIB_ENCODING_DEFLATE, 1)), "\n";
/// try { zlib_encode('x', 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { zlib_encode('x', ZLIB_ENCODING_RAW, 11); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 4b4c4a4e042300
/// 78014b4c4a4e042300113d0373
/// zlib_encode(): Argument #2 ($encoding) must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE
/// zlib_encode(): Argument #3 ($level) must be between -1 and 9
/// ```
#[php_function]
fn zlib_encode(
    req: &mut Request,
    data: PhpString,
    encoding: i64,
    #[php(default = -1)] level: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    checked(req, level, 3, encoding, 2)?;
    encode(req, data.as_bytes(), encoding, level)
}

/// `php_zlib_inflate_rounds`: up to a hundred `inflate(Z_NO_FLUSH)`
/// calls, the buffer starting at the input's length, or `max` where that
/// is less, and growing by an eighth and one each round; `Z_MEM_ERROR`
/// once `max` is reached, and a stream still short of its end
/// `Z_DATA_ERROR`.
fn inflate_rounds(stream: &mut Inflate, input: &[u8], max: usize) -> Result<Vec<u8>, Status> {
    let mut size = if max != 0 && max < input.len() {
        max
    } else {
        input.len()
    };
    let mut out: Vec<u8> = Vec::new();
    let (mut read, mut used, mut round) = (0usize, 0usize, 0u32);
    let status = loop {
        let status = if max != 0 && max <= used {
            Status::MemError
        } else {
            out.resize(size, 0);
            let step = stream.inflate(&input[read..], &mut out[used..size], Flush::None);
            read += step.read;
            used += step.written;
            size += (size >> 3) + 1;
            step.status
        };
        round += 1;
        let again = status == Status::BufError || (status == Status::Ok && read < input.len());
        if !again || round >= 100 {
            break status;
        }
    };
    match status {
        Status::StreamEnd => {
            out.truncate(used);
            Ok(out)
        }
        Status::Ok => Err(Status::DataError),
        other => Err(other),
    }
}

/// `php_zlib_decode`: the input with the NUL php's strings end in, which
/// zlib is handed too, inflated in rounds; under `PHP_ZLIB_ENCODING_ANY`
/// a data error tries again as raw deflate. Anything short of the
/// stream's end is php's warning with zlib's text.
fn decode(
    req: &mut Request,
    data: &[u8],
    mut encoding: i64,
    max: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    if max < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    let mut status = Status::DataError;
    if !data.is_empty() {
        let mut input = Vec::with_capacity(data.len() + 1);
        input.extend_from_slice(data);
        input.push(0);
        loop {
            let mut stream = match Inflate::new(encoding) {
                Ok(stream) => stream,
                Err(refused) => {
                    status = refused;
                    break;
                }
            };
            match inflate_rounds(&mut stream, &input, max) {
                Ok(out) => return Ok(OrFalse::Value(PhpString::new(req, &out)?)),
                Err(Status::DataError) if encoding == ENCODING_ANY => encoding = ZLIB_ENCODING_RAW,
                Err(failed) => {
                    status = failed;
                    break;
                }
            }
        }
    }
    docref(req, Level::Warning, &status.message());
    Ok(OrFalse::False)
}

/// `gzuncompress(string $data, int $max_length = 0): string|false`.
///
/// ```php
/// $c = gzcompress(str_repeat('abc', 100));
/// var_dump(gzuncompress($c) === str_repeat('abc', 100), gzuncompress($c, 300) === str_repeat('abc', 100));
/// var_dump(gzuncompress($c, 10), gzuncompress('garbage'), gzuncompress(''));
/// try { gzuncompress($c, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
///
/// Warning: gzuncompress(): insufficient memory in %s on line 4
///
/// Warning: gzuncompress(): data error in %s on line 4
///
/// Warning: gzuncompress(): data error in %s on line 4
/// bool(false)
/// bool(false)
/// bool(false)
/// gzuncompress(): Argument #2 ($max_length) must be greater than or equal to 0
/// ```
#[php_function]
fn gzuncompress(
    req: &mut Request,
    data: PhpString,
    #[php(default = 0)] max_length: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    decode(req, data.as_bytes(), ZLIB_ENCODING_DEFLATE, max_length)
}

/// `gzinflate(string $data, int $max_length = 0): string|false`.
///
/// ```php
/// var_dump(gzinflate(gzdeflate('hello world')), gzinflate(gzdeflate(str_repeat('x', 1000)), 999));
/// var_dump(gzinflate(gzcompress('hello')));
/// ```
/// ```expect
/// string(11) "hello world"
/// string(1000) "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
///
/// Warning: gzinflate(): data error in %s on line 3
/// bool(false)
/// ```
#[php_function]
fn gzinflate(
    req: &mut Request,
    data: PhpString,
    #[php(default = 0)] max_length: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    decode(req, data.as_bytes(), ZLIB_ENCODING_RAW, max_length)
}

/// `gzdecode(string $data, int $max_length = 0): string|false`.
///
/// ```php
/// var_dump(gzdecode(gzencode('hello world')), gzdecode(gzdeflate('hello')), gzdecode(gzencode('abc') . gzencode('def')));
/// ```
/// ```expect
/// Warning: gzdecode(): data error in %s on line 2
/// string(11) "hello world"
/// bool(false)
/// string(3) "abc"
/// ```
#[php_function]
fn gzdecode(
    req: &mut Request,
    data: PhpString,
    #[php(default = 0)] max_length: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    decode(req, data.as_bytes(), ZLIB_ENCODING_GZIP, max_length)
}

/// `zlib_decode(string $data, int $max_length = 0): string|false`: a
/// gzip or zlib header read as it comes, and raw deflate where neither
/// is there.
///
/// ```php
/// var_dump(zlib_decode(gzencode('one')), zlib_decode(gzcompress('two')), zlib_decode(gzdeflate('three')), zlib_decode('nope'));
/// ```
/// ```expect
/// Warning: zlib_decode(): data error in %s on line 2
/// string(3) "one"
/// string(3) "two"
/// string(5) "three"
/// bool(false)
/// ```
#[php_function]
fn zlib_decode(
    req: &mut Request,
    data: PhpString,
    #[php(default = 0)] max_length: i64,
) -> Result<OrFalse<PhpString>, Thrown> {
    decode(req, data.as_bytes(), ENCODING_ANY, max_length)
}
