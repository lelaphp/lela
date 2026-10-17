//! `bz2.c`'s two one-shot natives: `bzcompress`, one
//! `BZ2_bzBuffToBuffCompress` into php's worst-case buffer, and
//! `bzdecompress`, php's own loop over `BZ2_bzDecompress` that grows its
//! buffer by the input's length each time the library asks for more.

use php_ext::abi::{OrFalse, Union};
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{FreshString, Owner, PhpString};

use crate::ffi::{Action, Code, Compress, Decompress};

/// `bzcompress(string $data, int $block_size = 4, int $work_factor = 0):
/// string|int`: the data compressed at the block size and work factor
/// asked for, or the library's error number. The buffer is php's, the
/// input's length and a hundredth more and 600 bytes, and an input whose
/// buffer would pass `UINT_MAX` is refused.
///
/// Every block size, each at work factors 0, 30 and 250 over a text and
/// over one byte repeated, which the work factor steers to the fallback
/// sort, held to the oracle's bytes by their digests:
///
/// ```php
/// $text = str_repeat("Life it seems, will fade away\nDrifting further everyday\n", 400) . implode(',', range(1, 3000));
/// $run = str_repeat('a', 250000) . str_repeat('ab', 50000);
/// for ($b = 1; $b <= 9; $b++) {
///     foreach ([0, 30, 250] as $w) {
///         $t = bzcompress($text, $b, $w);
///         $r = bzcompress($run, $b, $w);
///         echo $b, ' ', $w, ' ', strlen($t), ' ', md5($t), ' ', strlen($r), ' ', md5($r), "\n";
///     }
/// }
/// var_dump(bin2hex(bzcompress('')), bin2hex(bzcompress('hello world', 9)), bin2hex(bzcompress('a', 1, 250)));
/// ```
/// ```expect
/// 1 0 3757 2b02c6f3a308d04ba3c3e025b9560cdf 80 ec7f8a2b0b1f3bf5e68289d5eff026a0
/// 1 30 3757 2b02c6f3a308d04ba3c3e025b9560cdf 80 ec7f8a2b0b1f3bf5e68289d5eff026a0
/// 1 250 3757 2b02c6f3a308d04ba3c3e025b9560cdf 80 ec7f8a2b0b1f3bf5e68289d5eff026a0
/// 2 0 3757 eac907043d55bfda28bfc28367be1127 52 2f805a7fa2a6142e4888d0bed81b58e1
/// 2 30 3757 eac907043d55bfda28bfc28367be1127 52 2f805a7fa2a6142e4888d0bed81b58e1
/// 2 250 3757 eac907043d55bfda28bfc28367be1127 52 2f805a7fa2a6142e4888d0bed81b58e1
/// 3 0 3757 5bea07e6448d85dca94e2c5bdecaad71 52 56f21196d9f336d6040d19059437f93b
/// 3 30 3757 5bea07e6448d85dca94e2c5bdecaad71 52 56f21196d9f336d6040d19059437f93b
/// 3 250 3757 5bea07e6448d85dca94e2c5bdecaad71 52 56f21196d9f336d6040d19059437f93b
/// 4 0 3757 6bf7a4dcef5d12bebdeb7bc3a055eeab 52 751c5ce1a18d9ed9a5d596458f506c2e
/// 4 30 3757 6bf7a4dcef5d12bebdeb7bc3a055eeab 52 751c5ce1a18d9ed9a5d596458f506c2e
/// 4 250 3757 6bf7a4dcef5d12bebdeb7bc3a055eeab 52 751c5ce1a18d9ed9a5d596458f506c2e
/// 5 0 3757 a4a6a43cd6b699484dc1256c8da92a21 52 190d6bb1e427e151eff22e09a9fe7db8
/// 5 30 3757 a4a6a43cd6b699484dc1256c8da92a21 52 190d6bb1e427e151eff22e09a9fe7db8
/// 5 250 3757 a4a6a43cd6b699484dc1256c8da92a21 52 190d6bb1e427e151eff22e09a9fe7db8
/// 6 0 3757 b68a2879e852779754a63e61f6c646a9 52 276a9f0c4d8a62022db77e20296decb5
/// 6 30 3757 b68a2879e852779754a63e61f6c646a9 52 276a9f0c4d8a62022db77e20296decb5
/// 6 250 3757 b68a2879e852779754a63e61f6c646a9 52 276a9f0c4d8a62022db77e20296decb5
/// 7 0 3757 c2d4ca5548d77872cc5957038488ecb0 52 5c0f98ab4d6fe769b3bd81a5a8b6c8e8
/// 7 30 3757 c2d4ca5548d77872cc5957038488ecb0 52 5c0f98ab4d6fe769b3bd81a5a8b6c8e8
/// 7 250 3757 c2d4ca5548d77872cc5957038488ecb0 52 5c0f98ab4d6fe769b3bd81a5a8b6c8e8
/// 8 0 3757 63e6f0a6d76550a08c06381abe1a1a9d 52 f73ba3d4f7720cb1f79e0c6157934257
/// 8 30 3757 63e6f0a6d76550a08c06381abe1a1a9d 52 f73ba3d4f7720cb1f79e0c6157934257
/// 8 250 3757 63e6f0a6d76550a08c06381abe1a1a9d 52 f73ba3d4f7720cb1f79e0c6157934257
/// 9 0 3757 fb470660d0aa659337dca78456732cc7 52 19688bb8c1a8cc113787373f5208d481
/// 9 30 3757 fb470660d0aa659337dca78456732cc7 52 19688bb8c1a8cc113787373f5208d481
/// 9 250 3757 fb470660d0aa659337dca78456732cc7 52 19688bb8c1a8cc113787373f5208d481
/// string(28) "425a683417724538509000000000"
/// string(96) "425a683931415926535944f7137800000191804000064490802000220334843021b68154278bb9229c2848227b89bc00"
/// string(74) "425a683131415926535919939b6b00000001002000200021184682ee48a70a120332736d60"
/// ```
///
/// The refusals, with php's texts:
///
/// ```php
/// foreach ([[0, 0], [10, 0], [4, -1], [4, 251]] as [$b, $w]) {
///     try { bzcompress('x', $b, $w); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// var_dump(bzcompress(1, 1, 1) === bzcompress('1', 1, 1));
/// ```
/// ```expect
/// bzcompress(): Argument #2 ($block_size) must be between 1 and 9
/// bzcompress(): Argument #2 ($block_size) must be between 1 and 9
/// bzcompress(): Argument #3 ($work_factor) must be between 0 and 250
/// bzcompress(): Argument #3 ($work_factor) must be between 0 and 250
/// bool(true)
/// ```
///
/// `bzcompress` and `bzdecompress` leave `bzalloc` null
/// (`Z/ext/bz2/bz2.c:521-522`), so the library's state is on its own
/// allocator and the request's heap never counts it, where a filter's is
/// counted ([`crate::filter::factory`]):
///
/// ```php
/// $s = str_repeat('abc', 1000); $z = bzcompress($s);
/// $m = memory_get_usage(); memory_reset_peak_usage(); $y = bzcompress($s, 9); var_dump(memory_get_peak_usage() - $m < 65536);
/// $m = memory_get_usage(); memory_reset_peak_usage(); $x = bzdecompress($z); var_dump(memory_get_peak_usage() - $m < 65536);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
///
/// The worst-case destination stands on the request's heap while the
/// library compresses, as php's `zend_string_alloc` holds it, so the
/// peak reads php's:
///
/// ```php
/// $s = str_repeat('abc', 1000);
/// $m = memory_get_usage(); memory_reset_peak_usage(); $y = bzcompress($s, 9); echo memory_get_peak_usage() - $m, "\n";
/// ```
/// ```expect exit=0
/// 4096
/// ```
#[php_function]
fn bzcompress(
    req: &mut Request,
    data: PhpString,
    #[php(default = 4)] block_size: i64,
    #[php(default = 0)] work_factor: i64,
) -> Result<Union<Owner<PhpString>, i64>, Thrown> {
    if !(1..=9).contains(&block_size) {
        return Err(arg_value_error(req, 2, "be between 1 and 9"));
    }
    if !(0..=250).contains(&work_factor) {
        return Err(arg_value_error(req, 3, "be between 0 and 250"));
    }
    let source = data.as_bytes();
    let limit = usize::try_from(u32::MAX).unwrap_or(usize::MAX);
    let chunk = source
        .len()
        .checked_add(source.len() / 100 + 600)
        .filter(|&chunk| chunk <= limit);
    let Some(chunk) = chunk else {
        let tail = format!("have a length less than or equal to {limit}");
        return Err(arg_value_error(req, 1, &tail));
    };
    // The two are in range, so each is an `int`.
    let (block_size, work_factor) = (
        i32::try_from(block_size).unwrap_or(9),
        i32::try_from(work_factor).unwrap_or(0),
    );
    // `zend_string_alloc(dest_len, 0)` before the library is asked, so
    // the worst case stands on the request's heap while it compresses,
    // and `zend_string_truncate` to what it wrote.
    let mut dest = FreshString::new(req, chunk)?;
    match compress_buffer(source, dest.bytes_mut(), block_size, work_factor) {
        Ok(written) => Ok(Union::A(dest.truncate(req, written)?.finish())),
        Err(code) => {
            dest.free(req);
            Ok(Union::B(code.number()))
        }
    }
}

/// `BZ2_bzBuffToBuffCompress` into `dest`: one `BZ_FINISH` call over the
/// whole input, how many bytes it wrote, and `BZ_OUTBUFF_FULL` where it
/// ends short of the stream's end.
fn compress_buffer(
    source: &[u8],
    dest: &mut [u8],
    block_size: i32,
    work_factor: i32,
) -> Result<usize, Code> {
    let mut stream = Compress::new(block_size, work_factor)?;
    let step = stream.compress(source, dest, Action::Finish);
    match step.code {
        Code::StreamEnd => Ok(step.written),
        Code::FinishOk => Err(Code::OutbuffFull),
        failed => Err(failed),
    }
}

/// `bzdecompress(string $data, bool $use_less_memory = false):
/// string|int|false`: php's loop. The buffer starts at twice the input's
/// length and grows by the input's length each time the library answers
/// `BZ_OK` with input left; the string is what was written when it
/// answers `BZ_STREAM_END`, or `BZ_OK` with its input read, a truncated
/// stream's prefix included, and the error number otherwise; `false`
/// where the stream cannot be made.
///
/// ```php
/// $text = str_repeat("Getting lost within myself\n", 5000);
/// $z = bzcompress($text, 9);
/// var_dump(bzdecompress($z) === $text, bzdecompress($z, true) === $text);
/// var_dump(bzdecompress(''), bzdecompress('1'), bzdecompress('BZh9'), bzdecompress(substr($z, 0, 40)));
/// $bad = $z; $bad[20] = chr(ord($bad[20]) ^ 0xff);
/// var_dump(bzdecompress($bad));
/// $two = bzcompress('one') . bzcompress('two');
/// var_dump(bzdecompress($two));
/// $zero = bzcompress(str_repeat("\0", 1 << 20));
/// var_dump(strlen($zero), strlen(bzdecompress($zero)));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(0) ""
/// int(-5)
/// string(0) ""
/// string(0) ""
/// int(-4)
/// string(3) "one"
/// int(45)
/// int(1048576)
/// ```
#[php_function]
fn bzdecompress(
    req: &mut Request,
    data: PhpString,
    #[php(default = false)] use_less_memory: bool,
) -> Result<Union<Owner<PhpString>, OrFalse<i64>>, Thrown> {
    let Ok(mut stream) = Decompress::new(use_less_memory) else {
        return Ok(Union::B(OrFalse::False));
    };
    let source = data.as_bytes();
    let mut dest = vec![0; source.len().saturating_mul(2)];
    let (mut read, mut size) = (0, 0);
    let code = loop {
        let step = stream.decompress(&source[read..], &mut dest[size..]);
        read += step.read;
        size += step.written;
        if step.code != Code::Ok || read == source.len() {
            break step.code;
        }
        dest.resize(size + source.len(), 0);
    };
    match code {
        Code::Ok | Code::StreamEnd => {
            dest.truncate(size);
            Ok(Union::A(PhpString::new(req, &dest)?))
        }
        failed => Ok(Union::B(OrFalse::Value(failed.number()))),
    }
}
