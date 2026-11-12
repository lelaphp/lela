//! `Z/ext/standard/filters.c`'s `convert.*` family: the four converters
//! `php_conv_open` makes, `php_conv_base64_encode`, `_decode`,
//! `php_conv_qprint_encode` and `_decode`, each a state machine over the
//! bytes of one bucket that keeps what it has not finished for the next,
//! and `strfilter_convert_filter`, which winds every bucket through one.
//!
//! php's converters write into a buffer of fixed size and answer
//! `PHP_CONV_ERR_TOO_BIG` where it fills, which
//! `strfilter_convert_append_bucket` answers by doubling it and calling
//! again with the converter's state kept; the bytes that come out are the
//! same as one call into a buffer that grows, which is what these write
//! into. None of the four answers `PHP_CONV_ERR_MORE`, so the stub php
//! keeps for a converter that does is never filled, and is not here.

use php_ext::abi::EngineState;
use php_ext::coerce::{to_long, truthy, with_string};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{
    Brigade, FilterFlags, FilterStatus, MadeFilter, Request, Stream, StreamFilter,
};
use php_ext::values::{Allocation, Emalloc, Malloc, PhpArray, Value};

/// `php_conv_err_t` less `PHP_CONV_ERR_SUCCESS`, the `Ok`, and the three
/// no converter here answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConvErr {
    Unknown,
    InvalidSeq,
    UnexpectedEos,
}

/// `b64_tbl_enc`, the alphabet.
const B64_ENC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `b64_tbl_enc[i]`, whose 256 entries are the alphabet four times.
fn b64(i: u32) -> u8 {
    B64_ENC[(i & 0x3f) as usize]
}

/// `b64_tbl_dec`: a digit's six bits, 64 for a byte that is none, and 128
/// for `=`.
fn b64_dec(c: u8) -> u32 {
    match c {
        b'A'..=b'Z' => u32::from(c - b'A'),
        b'a'..=b'z' => u32::from(c - b'a') + 26,
        b'0'..=b'9' => u32::from(c - b'0') + 52,
        b'+' => 62,
        b'/' => 63,
        b'=' => 128,
        _ => 64,
    }
}

/// `php_conv_base64_encode`: the line break and its length, the up to
/// two bytes left from the last call, and the room left on the line.
#[derive(EngineState)]
struct Base64Encode {
    lbchars: Option<Vec<u8>>,
    erem: [u8; 3],
    erem_len: usize,
    line_ccnt: u32,
    line_len: u32,
}

impl Base64Encode {
    /// `line_ccnt < 4 && lbchars != NULL`: the line break written where
    /// the line has no room for four more digits.
    fn wrap(&self, line_ccnt: &mut u32, out: &mut Vec<u8>) {
        if let Some(lb) = &self.lbchars
            && *line_ccnt < 4
        {
            out.extend_from_slice(lb);
            *line_ccnt = self.line_len;
        }
    }

    /// `php_conv_base64_encode_flush` (`Z/ext/standard/filters.c:255`):
    /// the bytes left padded with `=`.
    fn flush(&mut self, out: &mut Vec<u8>) -> Result<(), ConvErr> {
        let mut line_ccnt = self.line_ccnt;
        let e0 = u32::from(self.erem[0]);
        let e1 = u32::from(self.erem[1]);
        let answer = match self.erem_len {
            0 => Ok(()),
            1 => {
                self.wrap(&mut line_ccnt, out);
                out.extend_from_slice(&[b64(e0 >> 2), b64(e0 << 4), b'=', b'=']);
                self.erem_len = 0;
                line_ccnt = line_ccnt.wrapping_sub(4);
                Ok(())
            }
            2 => {
                self.wrap(&mut line_ccnt, out);
                out.extend_from_slice(&[
                    b64(e0 >> 2),
                    b64((e0 << 4) | (e1 >> 4)),
                    b64(e1 << 2),
                    b'=',
                ]);
                self.erem_len = 0;
                line_ccnt = line_ccnt.wrapping_sub(4);
                Ok(())
            }
            _ => Err(ConvErr::Unknown),
        };
        self.line_ccnt = line_ccnt;
        answer
    }

    /// `php_conv_base64_encode_convert` (`Z/ext/standard/filters.c:328`):
    /// the bytes left from the last call completed first, then every
    /// three bytes four digits, and what is left over kept.
    fn convert(&mut self, input: &[u8], out: &mut Vec<u8>) -> usize {
        let mut ps = 0;
        let mut line_ccnt = self.line_ccnt;
        let e0 = u32::from(self.erem[0]);
        let e1 = u32::from(self.erem[1]);
        let at = |i: usize| u32::from(input[i]);
        match self.erem_len {
            1 if input.len() >= 2 => {
                self.wrap(&mut line_ccnt, out);
                out.extend_from_slice(&[
                    b64(e0 >> 2),
                    b64((e0 << 4) | (at(0) >> 4)),
                    b64((at(0) << 2) | (at(1) >> 6)),
                    b64(at(1)),
                ]);
                ps = 2;
                self.erem_len = 0;
                line_ccnt = line_ccnt.wrapping_sub(4);
            }
            2 if !input.is_empty() => {
                self.wrap(&mut line_ccnt, out);
                out.extend_from_slice(&[
                    b64(e0 >> 2),
                    b64((e0 << 4) | (e1 >> 4)),
                    b64((e1 << 2) | (at(0) >> 6)),
                    b64(at(0)),
                ]);
                ps = 1;
                self.erem_len = 0;
                line_ccnt = line_ccnt.wrapping_sub(4);
            }
            _ => {}
        }
        while input.len() - ps >= 3 {
            self.wrap(&mut line_ccnt, out);
            out.extend_from_slice(&[
                b64(at(ps) >> 2),
                b64((at(ps) << 4) | (at(ps + 1) >> 4)),
                b64((at(ps + 1) << 2) | (at(ps + 2) >> 6)),
                b64(at(ps + 2)),
            ]);
            ps += 3;
            line_ccnt = line_ccnt.wrapping_sub(4);
        }
        for &b in &input[ps..] {
            if self.erem_len < self.erem.len() {
                self.erem[self.erem_len] = b;
                self.erem_len += 1;
            }
        }
        self.line_ccnt = line_ccnt;
        input.len()
    }
}

/// `php_conv_base64_decode`: the bits read and not yet a byte, the
/// `=` seen, and whether the padding ended the data.
#[derive(Default, EngineState)]
struct Base64Decode {
    urem: u32,
    urem_nbits: u32,
    ustat: u32,
    eos: bool,
}

/// `bmask(a)`, the low `a` bits.
const fn bmask(a: u32) -> u32 {
    0xffff >> (16 - a)
}

impl Base64Decode {
    /// `php_conv_base64_decode_convert` (`Z/ext/standard/filters.c:489`):
    /// eight bits a byte, a digit after `=` or a `=` where a byte is
    /// half made php's invalid sequence; how many bytes it read.
    fn convert(&mut self, input: &[u8], out: &mut Vec<u8>) -> (usize, Result<(), ConvErr>) {
        const NBITSOF_PACK: u32 = 8;
        let mut err = Ok(());
        let mut ps = 0;
        let mut urem = self.urem;
        let mut urem_nbits = self.urem_nbits;
        let mut ustat = self.ustat;
        let mut pack: u32 = 0;
        let mut pack_bcnt = NBITSOF_PACK;
        loop {
            if pack_bcnt >= urem_nbits {
                pack_bcnt -= urem_nbits;
                pack |= urem << pack_bcnt;
                urem_nbits = 0;
            } else {
                urem_nbits -= pack_bcnt;
                pack |= urem >> urem_nbits;
                urem &= bmask(urem_nbits);
                pack_bcnt = 0;
            }
            if pack_bcnt > 0 {
                let Some(&c) = input.get(ps) else {
                    break;
                };
                let i = b64_dec(c);
                ps += 1;
                ustat |= i & 0x80;
                if i & 0xc0 == 0 {
                    if ustat != 0 {
                        err = Err(ConvErr::InvalidSeq);
                        break;
                    }
                    if 6 <= pack_bcnt {
                        pack_bcnt -= 6;
                        pack |= i << pack_bcnt;
                        urem = 0;
                    } else {
                        urem_nbits = 6 - pack_bcnt;
                        pack |= i >> urem_nbits;
                        urem = i & bmask(urem_nbits);
                        pack_bcnt = 0;
                    }
                } else if ustat != 0 {
                    if pack_bcnt == 8 || pack_bcnt == 2 {
                        err = Err(ConvErr::InvalidSeq);
                        break;
                    }
                    self.eos = true;
                }
            }
            if (pack_bcnt | ustat) == 0 {
                out.push((pack & 0xff) as u8);
                pack = 0;
                pack_bcnt = NBITSOF_PACK;
            }
        }
        if urem_nbits >= pack_bcnt {
            urem |= pack << (urem_nbits - pack_bcnt);
        } else {
            urem |= pack >> (pack_bcnt - urem_nbits);
        }
        urem_nbits += NBITSOF_PACK - pack_bcnt;
        self.urem = urem;
        self.urem_nbits = urem_nbits;
        self.ustat = ustat;
        (ps, err)
    }

    /// The call with no input: bits left over without the padding that
    /// ends them is php's unexpected end of stream.
    fn flush(&self) -> Result<(), ConvErr> {
        if self.eos || self.urem_nbits == 0 {
            Ok(())
        } else {
            Err(ConvErr::UnexpectedEos)
        }
    }
}

/// `PHP_CONV_QPRINT_OPT_BINARY` and `PHP_CONV_QPRINT_OPT_FORCE_ENCODE_FIRST`.
#[derive(Default, Clone, Copy, EngineState)]
struct QprintOpts {
    binary: bool,
    force_encode_first: bool,
}

/// `php_conv_qprint_encode`: the line break, the options, the room left
/// on the line, and how much of a line break the input has matched
/// (`lb_cnt`) and given back (`lb_ptr`).
#[derive(EngineState)]
struct QprintEncode {
    lbchars: Option<Vec<u8>>,
    opts: QprintOpts,
    line_ccnt: u32,
    line_len: u32,
    lb_ptr: usize,
    lb_cnt: usize,
}

/// `qp_digits`.
const QP_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

impl QprintEncode {
    /// A soft line break, `=` and the line break, where `lbchars` is set.
    fn soft_break(&self, line_ccnt: &mut u32, out: &mut Vec<u8>, room: u32) {
        if let Some(lb) = &self.lbchars
            && *line_ccnt < room
        {
            out.push(b'=');
            out.extend_from_slice(lb);
            *line_ccnt = self.line_len;
        }
    }

    /// `php_conv_qprint_encode_convert` (`Z/ext/standard/filters.c:642`):
    /// a line break in the input copied as it is, white space before one
    /// encoded, printable bytes as they are and every other byte as `=`
    /// and two hex digits, a soft line break where the line is full.
    #[allow(clippy::too_many_lines)] // one function in php, ported as one
    fn convert(&mut self, input: &[u8], out: &mut Vec<u8>) -> usize {
        let lbchars = self.lbchars.clone().unwrap_or_default();
        let has_lb = self.lbchars.is_some();
        let mut line_ccnt = self.line_ccnt;
        let mut lb_ptr = self.lb_ptr;
        let mut lb_cnt = self.lb_cnt;
        let mut ps = 0usize;
        let mut trail_ws: u32 = 0;
        // `NEXT_CHAR` and `CONSUME_CHAR`.
        let next_char = |ps: usize, lb_ptr: usize, lb_cnt: usize| -> u8 {
            if lb_ptr < lb_cnt {
                lbchars[lb_ptr]
            } else {
                input.get(ps).copied().unwrap_or(0)
            }
        };
        let consume = |ps: &mut usize, lb_ptr: &mut usize, lb_cnt: &mut usize| {
            if *lb_ptr < *lb_cnt {
                *lb_ptr += 1;
            } else {
                *lb_cnt = 0;
                *lb_ptr = 0;
                *ps += 1;
            }
        };
        loop {
            let icnt = input.len() - ps;
            if !self.opts.binary && !lbchars.is_empty() && icnt > 0 && input[ps] == lbchars[lb_cnt]
            {
                lb_cnt += 1;
                if lb_cnt >= lbchars.len() {
                    out.extend_from_slice(&lbchars[..lb_cnt]);
                    line_ccnt = self.line_len;
                    lb_ptr = 0;
                    lb_cnt = 0;
                }
                ps += 1;
                continue;
            }
            if lb_ptr >= lb_cnt && icnt == 0 {
                break;
            }
            let c = next_char(ps, lb_ptr, lb_cnt);
            if !self.opts.binary && trail_ws == 0 && (c == b'\t' || c == b' ') {
                if line_ccnt < 2 && has_lb {
                    out.push(b'=');
                    out.extend_from_slice(&lbchars);
                    line_ccnt = self.line_len;
                } else {
                    // Whether the white space is at the end of a line.
                    if has_lb {
                        let mut lb_cnt2 = 0;
                        trail_ws = 1;
                        for &b in input[ps..].iter().take(icnt.saturating_sub(1)) {
                            if b == lbchars[lb_cnt2] {
                                lb_cnt2 += 1;
                                if lb_cnt2 >= lbchars.len() {
                                    break;
                                }
                            } else if lb_cnt2 != 0 || (b != b'\t' && b != b' ') {
                                trail_ws = 0;
                                break;
                            } else {
                                trail_ws += 1;
                            }
                        }
                    }
                    if trail_ws == 0 {
                        out.push(c);
                        line_ccnt = line_ccnt.wrapping_sub(1);
                        consume(&mut ps, &mut lb_ptr, &mut lb_cnt);
                    }
                }
            } else if (!self.opts.force_encode_first || line_ccnt < self.line_len)
                && ((33..=60).contains(&c) || (62..=126).contains(&c))
            {
                self.soft_break(&mut line_ccnt, out, 2);
                out.push(c);
                line_ccnt = line_ccnt.wrapping_sub(1);
                consume(&mut ps, &mut lb_ptr, &mut lb_cnt);
            } else {
                self.soft_break(&mut line_ccnt, out, 4);
                out.extend_from_slice(&[
                    b'=',
                    QP_DIGITS[usize::from(c >> 4)],
                    QP_DIGITS[usize::from(c & 0x0f)],
                ]);
                line_ccnt = line_ccnt.wrapping_sub(3);
                trail_ws = trail_ws.saturating_sub(1);
                consume(&mut ps, &mut lb_ptr, &mut lb_cnt);
            }
        }
        self.line_ccnt = line_ccnt;
        self.lb_ptr = lb_ptr;
        self.lb_cnt = lb_cnt;
        ps
    }
}

/// `php_conv_qprint_decode`: the line break, `None` to tell `\r\n`,
/// `\r` and `\n` apart as they come, and the scanner's state.
#[derive(EngineState)]
struct QprintDecode {
    lbchars: Option<Vec<u8>>,
    scan_stat: u32,
    next_char: u32,
    lb_ptr: usize,
    lb_cnt: usize,
}

impl QprintDecode {
    /// `php_conv_qprint_decode_convert` (`Z/ext/standard/filters.c:873`):
    /// `=` and two hex digits a byte, `=` before a line break a soft
    /// break dropped, and anything else after `=` php's invalid
    /// sequence; how many bytes it read. A hex digit is either case, as
    /// 8.5.12 reads a lowercase one:
    ///
    /// ```php
    /// foreach (['=cd', '=0a', '=da', '=CD', '=aB'] as $in) {
    ///     $m = fopen('php://memory', 'w+'); fwrite($m, $in); rewind($m);
    ///     stream_filter_append($m, 'convert.quoted-printable-decode', STREAM_FILTER_READ);
    ///     echo $in, ' => ', bin2hex(stream_get_contents($m)), "\n";
    /// }
    /// ```
    /// ```expect
    /// =cd => cd
    /// =0a => 0a
    /// =da => da
    /// =CD => cd
    /// =aB => ab
    /// ```
    #[allow(clippy::too_many_lines)] // one function in php, ported as one
    fn convert(&mut self, input: &[u8], out: &mut Vec<u8>) -> (usize, Result<(), ConvErr>) {
        let lbchars = self.lbchars.clone().unwrap_or_default();
        let auto = self.lbchars.is_none();
        let mut ps = 0usize;
        let mut scan_stat = self.scan_stat;
        let mut next_char = self.next_char;
        let mut lb_ptr = self.lb_ptr;
        let mut lb_cnt = self.lb_cnt;
        let mut err = Ok(());
        'scan: loop {
            let cur = input.get(ps).copied();
            match scan_stat {
                0 => {
                    let Some(c) = cur else { break };
                    if c == b'=' {
                        scan_stat = 1;
                    } else {
                        out.push(c);
                    }
                    ps += 1;
                }
                1..=3 => {
                    if scan_stat == 1 {
                        let Some(c) = cur else { break };
                        if c == b' ' || c == b'\t' {
                            scan_stat = 4;
                            ps += 1;
                            continue;
                        } else if auto && lb_cnt == 0 && c == b'\r' {
                            lb_cnt += 1;
                            scan_stat = 5;
                            ps += 1;
                            continue;
                        } else if auto && lb_cnt == 0 && c == b'\n' {
                            lb_cnt = 0;
                            lb_ptr = 0;
                            scan_stat = 0;
                            ps += 1;
                            continue;
                        } else if lb_cnt < lbchars.len() && c == lbchars[lb_cnt] {
                            lb_cnt += 1;
                            scan_stat = 5;
                            ps += 1;
                            continue;
                        }
                    }
                    if scan_stat <= 2 {
                        let Some(c) = input.get(ps).copied() else {
                            break;
                        };
                        if !c.is_ascii_hexdigit() {
                            err = Err(ConvErr::InvalidSeq);
                            break 'scan;
                        }
                        let digit = if c >= b'a' {
                            u32::from(c).wrapping_sub(0x57)
                        } else if c >= b'A' {
                            u32::from(c).wrapping_sub(0x37)
                        } else {
                            u32::from(c).wrapping_sub(0x30)
                        };
                        next_char = (next_char << 4) | digit;
                        scan_stat += 1;
                        ps += 1;
                        if scan_stat != 3 {
                            continue;
                        }
                    }
                    out.push((next_char & 0xff) as u8);
                    scan_stat = 0;
                }
                4 => {
                    let Some(c) = cur else { break };
                    if lb_cnt < lbchars.len() && c == lbchars[lb_cnt] {
                        lb_cnt += 1;
                        scan_stat = 5;
                    } else if c != b'\t' && c != b' ' {
                        err = Err(ConvErr::InvalidSeq);
                        break;
                    }
                    ps += 1;
                }
                5 => {
                    let Some(c) = cur else { break };
                    if auto && lb_cnt == 1 && c == b'\n' {
                        lb_cnt = 0;
                        lb_ptr = 0;
                        scan_stat = 0;
                        ps += 1;
                    } else if (auto && lb_cnt > 0) || lb_cnt >= lbchars.len() {
                        lb_cnt = 0;
                        lb_ptr = 0;
                        scan_stat = 0;
                    } else if c == lbchars[lb_cnt] {
                        lb_cnt += 1;
                        ps += 1;
                    } else {
                        scan_stat = 6;
                    }
                }
                _ => {
                    if lb_ptr < lb_cnt {
                        out.push(lbchars[lb_ptr]);
                        lb_ptr += 1;
                    } else {
                        scan_stat = 0;
                        lb_cnt = 0;
                        lb_ptr = 0;
                    }
                }
            }
        }
        self.scan_stat = scan_stat;
        self.next_char = next_char;
        self.lb_ptr = lb_ptr;
        self.lb_cnt = lb_cnt;
        (ps, err)
    }

    /// The call with no input: an `=` not finished is php's unexpected
    /// end of stream.
    fn flush(&self) -> Result<(), ConvErr> {
        if self.scan_stat == 0 {
            Ok(())
        } else {
            Err(ConvErr::UnexpectedEos)
        }
    }
}

/// `php_conv`, one of the four.
#[derive(EngineState)]
enum Conv {
    Base64Encode(Base64Encode),
    Base64Decode(Base64Decode),
    QprintEncode(QprintEncode),
    QprintDecode(QprintDecode),
}

impl Conv {
    /// `php_conv_convert` with input: how many bytes it read.
    fn convert(&mut self, input: &[u8], out: &mut Vec<u8>) -> (usize, Result<(), ConvErr>) {
        match self {
            Conv::Base64Encode(c) => (c.convert(input, out), Ok(())),
            Conv::Base64Decode(c) => c.convert(input, out),
            Conv::QprintEncode(c) => (c.convert(input, out), Ok(())),
            Conv::QprintDecode(c) => c.convert(input, out),
        }
    }

    /// `php_conv_convert(cd, NULL, NULL, ...)`, the flush at the end.
    fn flush(&mut self, out: &mut Vec<u8>) -> Result<(), ConvErr> {
        match self {
            Conv::Base64Encode(c) => c.flush(out),
            Conv::Base64Decode(c) => c.flush(),
            Conv::QprintEncode(_) => Ok(()),
            Conv::QprintDecode(c) => c.flush(),
        }
    }
}

/// One entry of the parameter array, `zend_hash_str_find`.
fn option(options: PhpArray, name: &[u8]) -> Option<Value> {
    options.find(name).map(|v| v.deref())
}

/// `GET_STR_PROP`: the entry as a string, `zval_get_tmp_string`.
fn string_option(
    req: &mut Request,
    options: PhpArray,
    name: &[u8],
) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(value) = option(options, name) else {
        return Ok(None);
    };
    with_string(req, &value, <[u8]>::to_vec).map(Some)
}

/// `GET_UINT_PROP`: the entry as a long, a negative one zero, cut to
/// php's `unsigned int`.
fn uint_option(req: &mut Request, options: PhpArray, name: &[u8]) -> Result<u32, Thrown> {
    let Some(value) = option(options, name) else {
        return Ok(0);
    };
    let long = to_long(req, &value)?;
    Ok(u64::try_from(long).map_or(0, |l| (l & 0xffff_ffff) as u32))
}

/// `GET_BOOL_PROP`.
fn bool_option(req: &mut Request, options: PhpArray, name: &[u8]) -> Result<bool, Thrown> {
    match option(options, name) {
        Some(value) => truthy(req, &value),
        None => Ok(false),
    }
}

/// `line-break-chars` and `line-length` as the two encoders read them: no
/// line break under a line of four, and `\r\n` where one is long enough
/// and none was given.
fn line_options(
    req: &mut Request,
    options: Option<PhpArray>,
) -> Result<(Option<Vec<u8>>, u32), Thrown> {
    let Some(options) = options else {
        return Ok((None, 0));
    };
    let lbchars = string_option(req, options, b"line-break-chars")?;
    let line_len = uint_option(req, options, b"line-length")?;
    if line_len < 4 {
        return Ok((None, line_len));
    }
    Ok((Some(lbchars.unwrap_or_else(|| b"\r\n".to_vec())), line_len))
}

/// `php_conv_open` (`Z/ext/standard/filters.c:1153`): the converter the
/// name after `convert.` asks for, with its options; `None` for a name
/// that asks for none of the four.
fn conv_open(
    req: &mut Request,
    mode: &[u8],
    options: Option<PhpArray>,
) -> Result<Option<Conv>, Thrown> {
    let conv = if mode.eq_ignore_ascii_case(b"base64-encode") {
        let (lbchars, line_len) = line_options(req, options)?;
        let line_len = if lbchars.is_some() { line_len } else { 0 };
        Conv::Base64Encode(Base64Encode {
            lbchars,
            erem: [0; 3],
            erem_len: 0,
            line_ccnt: line_len,
            line_len,
        })
    } else if mode.eq_ignore_ascii_case(b"base64-decode") {
        Conv::Base64Decode(Base64Decode::default())
    } else if mode.eq_ignore_ascii_case(b"quoted-printable-encode") {
        let (lbchars, line_len) = line_options(req, options)?;
        let mut opts = QprintOpts::default();
        if let Some(options) = options {
            opts.binary = bool_option(req, options, b"binary")?;
            opts.force_encode_first = bool_option(req, options, b"force-encode-first")?;
        }
        let line_len = if lbchars.is_some() { line_len } else { 0 };
        Conv::QprintEncode(QprintEncode {
            lbchars,
            opts,
            line_ccnt: line_len,
            line_len,
            lb_ptr: 0,
            lb_cnt: 0,
        })
    } else if mode.eq_ignore_ascii_case(b"quoted-printable-decode") {
        let lbchars = match options {
            Some(options) => string_option(req, options, b"line-break-chars")?,
            None => None,
        };
        Conv::QprintDecode(QprintDecode {
            lbchars,
            scan_stat: 0,
            next_char: 0,
            lb_ptr: 0,
            lb_cnt: 0,
        })
    } else {
        return Ok(None);
    };
    Ok(Some(conv))
}

/// `sizeof(php_convert_filter)` on the oracle's LP64.
const FILTER: usize = 160;

/// `sizeof` of each converter php allocates, on the oracle's LP64:
/// `php_conv_base64_encode`, `_base64_decode`, `_qprint_encode` and
/// `_qprint_decode`.
const BASE64_ENCODE: usize = 56;
const BASE64_DECODE: usize = 32;
const QPRINT_ENCODE: usize = 64;
const QPRINT_DECODE: usize = 56;

impl Conv {
    /// `sizeof` the converter php `pemalloc`s, and the line break it
    /// `pestrndup`s where it keeps one, all of its bytes.
    fn allocated(&self) -> (usize, Option<&[u8]>) {
        match self {
            Conv::Base64Encode(c) => (BASE64_ENCODE, c.lbchars.as_deref()),
            Conv::Base64Decode(_) => (BASE64_DECODE, None),
            Conv::QprintEncode(c) => (QPRINT_ENCODE, c.lbchars.as_deref()),
            Conv::QprintDecode(c) => (QPRINT_DECODE, c.lbchars.as_deref()),
        }
    }
}

/// `php_convert_filter`: the converter and the name the filter was asked
/// for, which its warnings print, and what php allocates for the two,
/// with the stream's persistence: the `php_convert_filter`, the
/// `pestrdup`'d name, the converter and its `pestrndup`'d line break, given back in
/// its `dtor`.
#[derive(EngineState)]
pub(super) struct ConvertFilter<A> {
    cd: Conv,
    filtername: String,
    held: Vec<A>,
}

impl<A: Allocation> ConvertFilter<A> {
    /// The filter over `cd`, with what php allocates for it: the
    /// `php_convert_filter`, the name of `name_len` bytes and its
    /// terminator, the converter and its line break. The `Err` is the
    /// memory limit's fatal.
    fn new(
        req: &mut Request,
        cd: Conv,
        filtername: String,
        name_len: usize,
    ) -> Result<ConvertFilter<A>, Thrown> {
        let (size, lbchars) = cd.allocated();
        let mut held = vec![
            A::new(req, FILTER)?,
            A::new(req, name_len + 1)?,
            A::new(req, size)?,
        ];
        if let Some(lbchars) = lbchars {
            held.push(A::new(req, lbchars.len() + 1)?);
        }
        Ok(ConvertFilter {
            cd,
            filtername,
            held,
        })
    }

    /// `strfilter_convert_append_bucket` (`Z/ext/standard/filters.c:1314`):
    /// one bucket's bytes converted, or the flush for `None`, what came
    /// out one bucket on `output`; a converter's error is php's warning
    /// and `false`. What was read is added to `consumed`.
    fn append_bucket(
        &mut self,
        req: &mut Request,
        output: &mut Brigade,
        input: Option<&[u8]>,
        consumed: &mut usize,
    ) -> bool {
        let mut out = Vec::new();
        let (read, answer) = match input {
            Some(bytes) => self.cd.convert(bytes, &mut out),
            // php's second switch has no arm for an unexpected end of
            // stream, so the flush that meets one ends as if it had not.
            None => match self.cd.flush(&mut out) {
                Err(ConvErr::UnexpectedEos) => (0, Ok(())),
                other => (0, other),
            },
        };
        if let Err(err) = answer {
            let what = match err {
                ConvErr::InvalidSeq => "invalid byte sequence",
                ConvErr::UnexpectedEos => "unexpected end of stream",
                ConvErr::Unknown => "unknown error",
            };
            let text = format!("Stream filter ({}): {what}", self.filtername);
            docref(req, Level::Warning, &text);
            return false;
        }
        if !out.is_empty() {
            output.push_back(out);
        }
        *consumed += read;
        true
    }
}

impl<A: Allocation> StreamFilter for ConvertFilter<A> {
    /// `strfilter_convert_dtor`: the converter's line break, the
    /// converter, the name and the filter given back.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        for block in self.held.drain(..).rev() {
            block.free(req);
        }
        Ok(())
    }

    /// `strfilter_convert_filter` (`Z/ext/standard/filters.c:1495`): each
    /// bucket converted, and the flush under `PSFS_FLAG_FLUSH_CLOSE`.
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
            if !self.append_bucket(req, output, Some(&bucket), &mut count) {
                return Ok(FilterStatus::ErrFatal);
            }
        }
        if flags == FilterFlags::FlushClose && !self.append_bucket(req, output, None, &mut count) {
            return Ok(FilterStatus::ErrFatal);
        }
        if let Some(n) = consumed {
            *n = count;
        }
        Ok(FilterStatus::PassOn)
    }
}

/// `strfilter_convert_create` (`Z/ext/standard/filters.c:1557`): the
/// converter the part of the name after its first `.` names, with the
/// parameters, which must be an array; a name that asks for none of the
/// four is refused without a word of its own.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "Hello, stream filters"); rewind($m);
/// stream_filter_append($m, 'convert.base64-encode', STREAM_FILTER_READ, ['line-length' => 8, 'line-break-chars' => "|"]);
/// var_dump(stream_get_contents($m));
/// $m = fopen('php://memory', 'w+'); fwrite($m, "SGVsbG8sIHdvcmxk"); rewind($m);
/// stream_filter_append($m, 'CONVERT.BASE64-DECODE');
/// var_dump(stream_get_contents($m));
/// foreach ([[], ['line-length' => 10], ['binary' => true], ['force-encode-first' => true, 'line-length' => 20]] as $p) {
///     $m = fopen('php://memory', 'w+'); fwrite($m, "caf\xe9 au lait \tend \r\nnext=line"); rewind($m);
///     stream_filter_append($m, 'convert.quoted-printable-encode', STREAM_FILTER_READ, $p);
///     var_dump(addcslashes(stream_get_contents($m), "\0..\37"));
/// }
/// $m = fopen('php://memory', 'w+'); fwrite($m, "caf=E9 au=\r\n lait=3D"); rewind($m);
/// stream_filter_append($m, 'convert.quoted-printable-decode');
/// var_dump(bin2hex(stream_get_contents($m)));
/// $m = fopen('php://memory', 'w+'); fwrite($m, "bad=ZZ"); rewind($m);
/// stream_filter_append($m, 'convert.quoted-printable-decode');
/// var_dump(stream_get_contents($m));
/// var_dump(stream_filter_append($m, 'convert.base64-encode', STREAM_FILTER_READ, 'x'));
/// var_dump(stream_filter_append($m, 'convert.nope'));
/// ```
/// ```expect
/// string(31) "SGVsbG8s|IHN0cmVh|bSBmaWx0|ZXJz"
///
/// Warning: stream_filter_append(): Unable to locate filter "CONVERT.BASE64-DECODE" in %s on line 6
/// string(16) "SGVsbG8sIHdvcmxk"
/// string(38) "caf=E9 au lait \tend =0D=0Anext=3Dline"
/// string(53) "caf=E9 au=\r\n lait \ten=\r\nd=20\r\nnext=3Dli=\r\nne"
/// string(47) "caf=E9=20au=20lait=20=09end=20=0D=0Anext=3Dline"
/// string(47) "=63af=E9 au lait \te=\r\nnd=20\r\n=6Eext=3Dline"
/// string(26) "636166e9206175206c6169743d"
///
/// Warning: stream_get_contents(): Stream filter (convert.quoted-printable-decode): invalid byte sequence in %s on line 18
/// string(0) ""
///
/// Warning: stream_filter_append(): Stream filter (convert.base64-encode): invalid filter parameter in %s on line 19
///
/// Warning: stream_filter_append(): Unable to create or locate filter "convert.base64-encode" in %s on line 19
/// bool(false)
///
/// Warning: stream_filter_append(): Unable to create or locate filter "convert.nope" in %s on line 20
/// bool(false)
/// ```
///
/// What php allocates for a filter is counted with the stream's
/// persistence, here and in `dechunk` and `consumed`: made, removed, and
/// made on a persistent stream, past a filter of the name made first.
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $a = pfsockopen(...explode(':', stream_socket_get_name($s, false)));
/// $cases = [['convert.base64-encode', ['line-length' => 8]], ['convert.base64-encode', ['line-length' => 8, 'line-break-chars' => str_repeat('-', 40)]], ['convert.base64-decode', []], ['convert.quoted-printable-encode', ['line-length' => 8]], ['convert.quoted-printable-decode', ['line-break-chars' => "\n"]], ['dechunk', null], ['consumed', null]];
/// foreach ($cases as [$name, $p]) {
///     $f = fopen('php://memory', 'w+');
///     $w = fopen('php://memory', 'w+'); stream_filter_append($w, $name, STREAM_FILTER_READ, $p);
///     $m = memory_get_usage(); $h = stream_filter_append($f, $name, STREAM_FILTER_READ, $p); $made = memory_get_usage() - $m;
///     $m = memory_get_usage(); stream_filter_remove($h); $gone = memory_get_usage() - $m;
///     $m = memory_get_usage(); $q = stream_filter_append($a, $name, STREAM_FILTER_READ, $p); echo $name, ' ', $made, ' ', $gone, ' ', memory_get_usage() - $m, "\n";
/// }
/// ```
/// ```expect
/// convert.base64-encode 368 -336 64
/// convert.base64-encode 368 -368 352
/// convert.base64-decode 296 -296 32
/// convert.quoted-printable-encode 352 -352 32
/// convert.quoted-printable-decode 344 -344 32
/// dechunk 96 -96 32
/// consumed 104 -104 32
/// ```
pub(super) fn create(
    req: &mut Request,
    name: &[u8],
    params: Option<&Value>,
    persistent: bool,
) -> Result<Option<MadeFilter>, Thrown> {
    let filtername = String::from_utf8_lossy(name).into_owned();
    let options = params.map(php_ext::values::Value::as_array);
    if options == Some(None) {
        let text = format!("Stream filter ({filtername}): invalid filter parameter");
        docref(req, Level::Warning, &text);
        return Ok(None);
    }
    let options = options.flatten();
    let Some(dot) = name.iter().position(|b| *b == b'.') else {
        return Ok(None);
    };
    let Some(cd) = conv_open(req, &name[dot + 1..], options)? else {
        return Ok(None);
    };
    Ok(Some(if persistent {
        let filter = ConvertFilter::<Malloc>::new(req, cd, filtername, name.len())?;
        MadeFilter::Kept(Box::new(filter))
    } else {
        let filter = ConvertFilter::<Emalloc>::new(req, cd, filtername, name.len())?;
        MadeFilter::Request(Box::new(filter))
    }))
}
