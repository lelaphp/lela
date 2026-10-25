//! `main/rfc1867.c` at the pin: a `multipart/form-data` body read part by
//! part through a buffer of `FILLUNIT` bytes as php reads it, each form
//! field registered into `$_POST`, each file written to a temporary file
//! and described in `$_FILES`, with php's limits, its warnings and its
//! quirks of a buffer filled from the host a block at a time: a line
//! longer than the buffer is handed out whole-buffer at a time, and a
//! boundary is looked for only in what the buffer holds. Under
//! `mbstring.encoding_translation` each part's names and value are
//! read in the encoding its header lines are detected in and converted
//! to the internal one, through the functions an extension handed the
//! engine ([`Translation`]).
//!
//! What php keeps of a body on the request's heap is kept there: the
//! buffer, the header lines kept, each word of a part's disposition, a
//! form field's value and the names a file is described under. php
//! 8.5.12 grows a value fallibly and stops at each `ZEND_HAS_OOM()`
//! once the memory limit was met, so a body past the limit ends the
//! request in the limit's fatal, raised by `sapi_handle_post`, and never
//! at the hard limit.

use crate::api::abi::{Encoding, Multibyte};
use crate::api::raising::{Cause, Level, Thrown, docref, throw};
use crate::api::request::temporary::TemporaryGuard;
use crate::api::request::{BodyLimits, Input, Request, Upload};
use crate::api::values::{Emalloc, Owner, PhpArray, PhpString, Value};
use crate::c_number::{c_str, is_space};

/// `FILLUNIT` (`Z/main/rfc1867.c:188`).
const FILLUNIT: usize = 1024 * 5;

/// `sizeof(multipart_buffer)` (`Z/main/rfc1867.c:190`).
const MULTIPART_BUFFER: usize = 72;

/// `sizeof(zend_llist_element) + sizeof(mime_header_entry) - 1`, what
/// `zend_llist_add_element` asks for each header line kept.
const HEADER_ELEMENT: usize = 39;

/// `MAX_SIZE_OF_INDEX + 1`, `sizeof("[full_path]") + 1`: what `lbuf`
/// holds past a file's field name.
const INDEX_ROOM: usize = 13;

/// `MAX_SIZE_ANONNAME` (`Z/main/rfc1867.c:58`).
const ANONYMOUS_NAME: usize = 33;

/// `UPLOAD_ERR_*` (`Z/main/rfc1867.h:30-37`).
const UPLOAD_ERROR_A: i64 = 1;
const UPLOAD_ERROR_B: i64 = 2;
const UPLOAD_ERROR_C: i64 = 3;
const UPLOAD_ERROR_D: i64 = 4;
const UPLOAD_ERROR_E: i64 = 6;
const UPLOAD_ERROR_F: i64 = 7;
const UPLOAD_ERROR_X: i64 = 8;

/// A `char *` php holds on the request's heap: `estrndup`'s copy of a
/// text, `emalloc(len + 1)`, or `emalloc(size)` where php asks for more
/// than the text fills (`substring_conf`), the text and its terminator
/// at the front.
struct Text {
    block: Emalloc,
    len: usize,
}

impl Text {
    /// `estrndup(bytes, len)`.
    fn dup(req: &mut Request, bytes: &[u8]) -> Result<Text, Thrown> {
        Text::sized(req, bytes.len() + 1, bytes)
    }

    /// `emalloc(size)` holding `bytes`, `size` past them.
    fn sized(req: &mut Request, size: usize, bytes: &[u8]) -> Result<Text, Thrown> {
        let mut block = Emalloc::new(req, size.max(bytes.len() + 1))?;
        block.bytes_mut()[..bytes.len()].copy_from_slice(bytes);
        Ok(Text {
            block,
            len: bytes.len(),
        })
    }

    /// The text.
    fn bytes(&self) -> &[u8] {
        &self.block.bytes()[..self.len]
    }

    /// `efree`.
    fn free(self, req: &mut Request) {
        self.block.free(req);
    }
}

/// `efree` of a `char *` php may hold.
fn free(req: &mut Request, text: Option<Text>) {
    if let Some(text) = text {
        text.free(req);
    }
}

/// `smart_string` (`Z/Zend/zend_smart_string.h`), a header line's value
/// as `multipart_buffer_headers` builds it: laid down at 255 bytes, and
/// grown to the page past what it holds once an append reaches its room
/// (`_smart_string_alloc`, `Z/Zend/zend_smart_str.c:206`).
#[derive(Default)]
struct SmartString {
    block: Option<Emalloc>,
    len: usize,
    room: usize,
}

impl SmartString {
    /// `SMART_STRING_START_LEN`.
    const START: usize = 255;

    /// `smart_string_appendl`.
    fn append(&mut self, req: &mut Request, bytes: &[u8]) -> Result<(), Thrown> {
        let more = bytes.len();
        let page = |len: usize| (len + 1).next_multiple_of(4096) - 1;
        let block = match self.block.take() {
            None => {
                self.len = 0;
                self.room = if more <= Self::START {
                    Self::START
                } else {
                    page(more)
                };
                Emalloc::new(req, self.room + 1)?
            }
            Some(block) if more >= self.room - self.len => {
                self.room = page(self.len + more);
                block.resize(req, self.room + 1)?
            }
            Some(block) => block,
        };
        let block = self.block.insert(block);
        block.bytes_mut()[self.len..self.len + more].copy_from_slice(bytes);
        self.len += more;
        Ok(())
    }

    /// Whether anything was appended, php's `buf_value.c`.
    fn started(&self) -> bool {
        self.block.is_some()
    }

    /// `smart_string_0` and the buffer handed on as the `char *` it is.
    fn into_text(self) -> Option<Text> {
        let len = self.len;
        self.block.map(|block| Text { block, len })
    }
}

/// One header line kept, its `mime_header_entry` in the
/// `zend_llist_element` that holds it: the name `estrdup`'d and the
/// value [`SmartString`] grew.
struct Header {
    key: Text,
    value: Text,
    element: Emalloc,
}

/// The part's header lines, php's `zend_llist header`.
#[derive(Default)]
struct Headers(Vec<Header>);

impl Headers {
    /// `php_mime_get_hdr_value`: the first line of `name`, in any case.
    fn find(&self, name: &[u8]) -> Option<&[u8]> {
        (self.0.iter())
            .find(|header| header.key.bytes().eq_ignore_ascii_case(name))
            .map(|header| header.value.bytes())
    }

    /// `zend_llist_add_element`.
    fn add(&mut self, req: &mut Request, key: Text, value: Text) -> Result<(), Thrown> {
        let element = Emalloc::new(req, HEADER_ELEMENT)?;
        self.0.push(Header {
            key,
            value,
            element,
        });
        Ok(())
    }

    /// `zend_llist_clean`: each line's name, value and element freed,
    /// first to last.
    fn clean(&mut self, req: &mut Request) {
        for header in std::mem::take(&mut self.0) {
            header.key.free(req);
            header.value.free(req);
            header.element.free(req);
        }
    }
}

/// `php_mime_should_save_hdr`: the two header lines php reads.
fn kept_header(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"Content-Disposition") || name.eq_ignore_ascii_case(b"Content-Type")
}

/// `multipart_buffer` (`Z/main/rfc1867.c:190`): the bytes read from the
/// host and not yet handed out, `buf_begin..buf_begin+bytes_in_buffer` of
/// a buffer of `bufsize`, and the two boundaries.
struct Buffer {
    bytes: Vec<u8>,
    begin: usize,
    len: usize,
    size: usize,
    boundary: Vec<u8>,
    boundary_next: Vec<u8>,
    /// The last header line read, which translation detects the part's
    /// encoding over (`self->input_encoding`, `Z/main/rfc1867.c:390`).
    last: Vec<u8>,
    /// What `multipart_buffer_new` asks the request's heap for: the
    /// struct, its buffer and the two boundaries `spprintf` writes.
    held: Vec<Emalloc>,
    /// The `post_max_size` the body is held to, the handler's or the
    /// `request_parse_body()` option's, which `fill_buffer` reads at
    /// 8.5.12.
    post_max_size: i64,
}

impl Buffer {
    /// `multipart_buffer_new`.
    fn new(req: &mut Request, boundary: &[u8], post_max_size: i64) -> Result<Buffer, Thrown> {
        let size = (boundary.len() + 6).max(FILLUNIT);
        let mut dashed = b"--".to_vec();
        dashed.extend_from_slice(boundary);
        let mut next = b"\n--".to_vec();
        next.extend_from_slice(boundary);
        let held = vec![
            Emalloc::new(req, MULTIPART_BUFFER)?,
            Emalloc::new(req, size + 1)?,
            Emalloc::printf_buffer(req, dashed.len())?,
            Emalloc::printf_buffer(req, next.len())?,
        ];
        Ok(Buffer {
            bytes: vec![0; size + 1],
            begin: 0,
            len: 0,
            size,
            boundary: dashed,
            boundary_next: next,
            last: Vec::new(),
            held,
            post_max_size,
        })
    }

    /// The boundaries, the buffer and the struct freed, as
    /// `fileupload_done` frees them.
    fn free(self, req: &mut Request) {
        for block in self.held.into_iter().rev() {
            block.free(req);
        }
    }

    /// The bytes held.
    fn held(&self) -> &[u8] {
        &self.bytes[self.begin..self.begin + self.len]
    }

    /// `fill_buffer`: what is held moved to the start, and the free room
    /// filled from the host; how much came. A body read past
    /// `post_max_size` already fills nothing more, as 8.5.12 stops it
    /// (`Z/main/rfc1867.c:227`), so a body whose `Content-Length` said
    /// less, or nothing, is cut where it passed the limit.
    fn fill(&mut self, req: &mut Request) -> usize {
        if self.len > 0 && self.begin != 0 {
            self.bytes.copy_within(self.begin..self.begin + self.len, 0);
        }
        self.begin = 0;
        if self.past_limit(req) {
            return 0;
        }
        let mut total = 0;
        let want = self.size - self.len;
        if want > 0 {
            let got = req.read_post(want);
            self.bytes[self.len..self.len + got.len()].copy_from_slice(&got);
            self.len += got.len();
            total += got.len();
        }
        total
    }

    /// Whether the body read so far, `SG(read_post_bytes)`, is past the
    /// `post_max_size` it is held to.
    fn past_limit(&self, req: &Request) -> bool {
        self.post_max_size > 0 && req.post_bytes_read() > self.post_max_size
    }

    /// `fileupload_done`'s warning at 8.5.12, `sapi_error`'s, for a body
    /// read past `post_max_size`, whatever stopped the parse.
    fn warn_past_limit(&self, req: &mut Request) {
        if self.past_limit(req) {
            let limit = self.post_max_size;
            req.note_error(
                Level::Warning,
                &format!(
                    "Actual POST length does not match Content-Length, and exceeds {limit} bytes"
                ),
            );
        }
    }

    /// `multipart_buffer_eof`.
    fn eof(&mut self, req: &mut Request) -> bool {
        self.len == 0 && self.fill(req) < 1
    }

    /// `next_line`: the bytes up to the next `\n`, a `\r` before it cut
    /// too; or, with none held and the buffer full, the whole buffer as a
    /// line; `None` otherwise.
    fn next_line(&mut self) -> Option<Vec<u8>> {
        let held = self.held();
        if let Some(at) = held.iter().position(|b| *b == b'\n') {
            let end = if at > 0 && held[at - 1] == b'\r' {
                at - 1
            } else {
                at
            };
            let line = held[..end].to_vec();
            self.begin += at + 1;
            self.len -= at + 1;
            return Some(line);
        }
        if self.len < self.size {
            return None;
        }
        let line = held.to_vec();
        self.len = 0;
        Some(line)
    }

    /// `get_line`.
    fn get_line(&mut self, req: &mut Request) -> Option<Vec<u8>> {
        if let Some(line) = self.next_line() {
            return Some(line);
        }
        self.fill(req);
        self.next_line()
    }

    /// `find_boundary`.
    fn find_boundary(&mut self, req: &mut Request) -> bool {
        while let Some(line) = self.get_line(req) {
            if c_str(&line) == self.boundary.as_slice() {
                return true;
            }
        }
        false
    }

    /// `multipart_buffer_headers` at 8.5.12: past the boundary, each
    /// header line up to the blank one, a line opening with white space
    /// or holding no colon added to the one before. Only a
    /// `Content-Disposition` and a `Content-Type` line are kept, the
    /// first of each; `false` where no boundary came, or once the memory
    /// limit was met.
    fn headers(&mut self, req: &mut Request, headers: &mut Headers) -> Result<bool, Thrown> {
        if !self.find_boundary(req) {
            return Ok(false);
        }
        let mut key: Option<Text> = None;
        let mut value = SmartString::default();
        while let Some(line) = self.get_line(req) {
            let line = c_str(&line).to_vec();
            if line.is_empty() {
                break;
            }
            self.last.clone_from(&line);
            let colon = if line.first().is_some_and(|b| is_space(*b)) {
                None
            } else {
                line.iter().position(|b| *b == b':')
            };
            if let Some(colon) = colon {
                if value.started()
                    && let Some(done) = key.take()
                    && let Some(text) = std::mem::take(&mut value).into_text()
                {
                    headers.add(req, done, text)?;
                }
                let name = &line[..colon];
                if kept_header(name) && headers.find(name).is_none() {
                    key = Some(Text::dup(req, name)?);
                    let mut at = colon + 1;
                    while line.get(at).is_some_and(|b| is_space(*b)) {
                        at += 1;
                    }
                    value.append(req, &line[at.min(line.len())..])?;
                } else {
                    key = None;
                }
            } else if value.started() {
                value.append(req, &line)?;
            } else {
                continue;
            }
            if req.has_oom() {
                free(req, key);
                free(req, value.into_text());
                return Ok(false);
            }
        }
        if value.started()
            && let Some(done) = key
            && let Some(text) = value.into_text()
        {
            headers.add(req, done, text)?;
        }
        Ok(true)
    }

    /// `multipart_buffer_read`: up to `bytes - 1` bytes up to where a
    /// boundary may begin, a `\r` before that boundary cut, and whether
    /// the boundary is wholly held (`end`).
    fn read(&mut self, req: &mut Request, bytes: usize, end: &mut bool) -> Vec<u8> {
        if bytes > self.len {
            self.fill(req);
        }
        let held = self.held();
        let bound = memstr(held, &self.boundary_next, true);
        let max = match bound {
            Some(at) => {
                if memstr(held, &self.boundary_next, false).is_some() {
                    *end = true;
                }
                at
            }
            None => held.len(),
        };
        let mut len = max.min(bytes - 1);
        if len == 0 {
            return Vec::new();
        }
        let mut out = held[..len].to_vec();
        if bound.is_some() && out.last() == Some(&b'\r') {
            out.pop();
            len -= 1;
        }
        self.len -= len;
        self.begin += len;
        out
    }

    /// `multipart_buffer_read_body` at 8.5.12: what `read` hands out, to
    /// the end of the part, in a buffer on the request's heap grown by
    /// each read, fallibly (`safe_ferealloc`); a growth the memory limit
    /// refuses ends the read, the delayed OOM recorded, and what was
    /// read is kept.
    fn read_body(&mut self, req: &mut Request) -> Result<Option<Text>, Thrown> {
        let mut out: Option<Emalloc> = None;
        let mut total = 0;
        let mut end = false;
        loop {
            let chunk = self.read(req, FILLUNIT, &mut end);
            if chunk.is_empty() {
                break;
            }
            let size = total + chunk.len() + 1;
            let grown = grow(req, &mut out, size)?;
            let Some(mut grown) = grown else {
                break;
            };
            grown.bytes_mut()[total..total + chunk.len()].copy_from_slice(&chunk);
            total += chunk.len();
            out = Some(grown);
        }
        Ok(out.map(|block| Text { block, len: total }))
    }
}

/// `php_ap_memstr`: where `needle` begins in `haystack`, and with
/// `partial` where a prefix of it ends the haystack.
fn memstr(haystack: &[u8], needle: &[u8], partial: bool) -> Option<usize> {
    let first = *needle.first()?;
    let mut at = 0;
    while at < haystack.len() {
        let found = haystack[at..].iter().position(|b| *b == first)?;
        at += found;
        let len = haystack.len() - at;
        let n = needle.len().min(len);
        if haystack[at..at + n] == needle[..n] && (partial || len >= needle.len()) {
            return Some(at);
        }
        at += 1;
    }
    None
}

/// `php_ap_getword`: the text up to `stop` outside quotes, a quote's
/// escaped quote kept inside it, and every `stop` after it passed over;
/// `php_mb_rfc1867_getword` where `width` walks a character at a time.
fn getword(line: &mut &[u8], stop: u8, width: &dyn Fn(u8) -> usize) -> Vec<u8> {
    let s = *line;
    let mut pos = 0;
    while pos < s.len() && s[pos] != stop {
        let quote = s[pos];
        if quote == b'"' || quote == b'\'' {
            pos += 1;
            while pos < s.len() && s[pos] != quote {
                if s[pos] == b'\\' && s.get(pos + 1) == Some(&quote) {
                    pos += 2;
                } else {
                    pos += 1;
                }
            }
            if pos < s.len() {
                pos += 1;
            }
        } else {
            pos += width(s[pos]).max(1);
        }
    }
    let pos = pos.min(s.len());
    if pos == s.len() {
        *line = &s[s.len()..];
        return s.to_vec();
    }
    let word = s[..pos].to_vec();
    let mut rest = pos;
    while rest < s.len() && s[rest] == stop {
        rest += width(s[rest]).max(1);
    }
    *line = &s[rest.min(s.len())..];
    word
}

/// `php_ap_getword_conf`: the value after its leading white space,
/// quoted or up to the next white space, an escaped quote or backslash
/// taken for itself; `php_mb_rfc1867_getword_conf` under `step`. With
/// it the length `substring_conf` is handed, which its `emalloc` asks
/// for, `None` for the `estrdup("")` of an empty value.
fn getword_conf(s: &[u8], step: &dyn Fn(u8) -> usize) -> (Vec<u8>, Option<usize>) {
    let start = s.iter().take_while(|b| is_space(**b)).count();
    let s = &s[start..];
    let Some(&first) = s.first() else {
        return (Vec::new(), None);
    };
    if first == b'"' || first == b'\'' {
        (substring_conf(&s[1..], first, step), Some(s.len() - 1))
    } else {
        let end = s.iter().position(|b| is_space(*b)).unwrap_or(s.len());
        (substring_conf(&s[..end], 0, step), Some(end))
    }
}

/// `substring_conf`: up to the closing `quote`, `\\` and `\<quote>` read
/// as the byte after the backslash, any other character copied whole
/// (`php_mb_rfc1867_substring_conf` under `step`).
fn substring_conf(s: &[u8], quote: u8, step: &dyn Fn(u8) -> usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() && (quote == 0 || s[i] != quote) && s[i] != 0 {
        if s[i] == b'\\'
            && s.get(i + 1)
                .is_some_and(|next| *next == b'\\' || (quote != 0 && *next == quote))
        {
            out.push(s[i + 1]);
            i += 2;
            continue;
        }
        let end = (i + step(s[i]).max(1)).min(s.len());
        out.extend_from_slice(&s[i..end]);
        i = end;
    }
    out
}

/// `php_ap_basename`: what follows the last `\` or `/`, found a
/// character at a time under `step` (`php_mb_rfc1867_basename` over
/// `php_mb_safe_strrchr`).
fn basename<'p>(path: &'p [u8], step: &dyn Fn(u8) -> usize) -> &'p [u8] {
    let mut last = None;
    let mut at = 0;
    while at < path.len() {
        if matches!(path[at], b'\\' | b'/') {
            last = Some(at);
        }
        at += step(path[at]).max(1);
    }
    match last {
        Some(at) => &path[at + 1..],
        None => path,
    }
}

/// `mbstring.encoding_translation` as rfc1867 asks it
/// (`php_rfc1867_set_multibyte_callbacks`): the functions bound to the
/// request, the encodings a part's header lines are detected among, and
/// the internal encoding the part is converted to.
struct Translation {
    functions: Box<dyn Multibyte>,
    http_input: Vec<Encoding>,
    internal: Option<Encoding>,
}

impl Translation {
    /// The request's, where it translates a form body.
    fn of(req: &mut Request) -> Option<Translation> {
        let functions = crate::multibyte::bind(req);
        let http_input = functions.http_input()?;
        let internal = functions.internal();
        Some(Translation {
            functions,
            http_input,
            internal,
        })
    }

    /// `text` in `input`, converted to the internal encoding where both
    /// stand and the conversion answers, as
    /// `zend_multibyte_encoding_converter` converts it: mbstring's
    /// converted string copied into `emalloc(len + 1)` and freed, then
    /// the text it replaces freed.
    fn convert(
        &self,
        req: &mut Request,
        text: Text,
        input: Option<Encoding>,
    ) -> Result<Text, Thrown> {
        let (Some(input), Some(internal)) = (input, self.internal) else {
            return Ok(text);
        };
        let Some(converted) = self.functions.convert(text.bytes(), internal, input) else {
            return Ok(text);
        };
        let (made, room) = converted_string(req, text.len, converted.len())?;
        let copy = Text::dup(req, &converted[..converted.len().min(room)]);
        made.free(req);
        let copy = copy?;
        text.free(req);
        Ok(copy)
    }
}

/// The string `mb_fast_convert` writes a conversion into
/// (`Z/ext/mbstring/libmbfl/mbfl/mbfl_convert.c:324` at 8.5.12): laid
/// down with room for the input's length (`mb_convert_buf_init`), and
/// grown by half again whenever the converted text fills it, fallibly
/// (`mb_convert_buf_ensure`, `Z/ext/mbstring/libmbfl/mbfl/mbfl_encoding.h:164`).
/// With it the room it reached, short of `converted` where the memory limit
/// refused a growth and the conversion stopped there.
fn converted_string(
    req: &mut Request,
    input: usize,
    converted: usize,
) -> Result<(Emalloc, usize), Thrown> {
    // `_ZSTR_STRUCT_SIZE(len)`: the string's 24-byte header, its bytes
    // and the terminator.
    let size = |room: usize| room + 25;
    let mut room = input;
    let mut made = Some(Emalloc::new(req, size(room))?);
    while room < converted {
        let grown_room = room + (room >> 1).max(1);
        let Some(grown) = grow(req, &mut made, size(grown_room))? else {
            break;
        };
        made = Some(grown);
        room = grown_room;
    }
    match made {
        Some(made) => Ok((made, room)),
        None => Ok((Emalloc::new(req, size(room))?, room)),
    }
}

/// `held` grown to `len` bytes fallibly, `safe_ferealloc(held, ...,
/// true)`, or laid down where nothing is held: `None` where the memory
/// limit refused it, the delayed OOM recorded and `held` left as it
/// stood, as php keeps a block whose growth was refused.
fn grow(
    req: &mut Request,
    held: &mut Option<Emalloc>,
    len: usize,
) -> Result<Option<Emalloc>, Thrown> {
    let taken = held.take();
    let mut kept = None;
    let grown = req.memory_mut().fallible(|req| match taken {
        Some(block) => block.try_resize(req, len).map_err(|(block, thrown)| {
            kept = Some(block);
            thrown
        }),
        None => Emalloc::new(req, len),
    });
    *held = kept;
    grown
}

/// `normalize_protected_variable`: the name less its leading spaces, a
/// space or dot before the first `[` read as `_`, and each index's
/// leading white space taken out.
fn normalize_protected(name: &[u8]) -> Vec<u8> {
    let name = &name[name.iter().take_while(|b| **b == b' ').count()..];
    let open = name.iter().position(|b| *b == b'[');
    let head = name.get(..open.unwrap_or(name.len())).unwrap_or_default();
    let mut out: Vec<u8> = (head.iter())
        .map(|b| if matches!(b, b' ' | b'.') { b'_' } else { *b })
        .collect();
    let Some(open) = open else {
        return out;
    };
    out.push(b'[');
    let mut index = open + 1;
    loop {
        while name
            .get(index)
            .is_some_and(|b| matches!(b, b' ' | b'\r' | b'\n' | b'\t'))
        {
            index += 1;
        }
        let end = name[index.min(name.len())..]
            .iter()
            .position(|b| *b == b']')
            .map_or(name.len(), |at| index + at + 1);
        out.extend_from_slice(&name[index.min(name.len())..end]);
        if name.get(end) == Some(&b'[') {
            out.push(b'[');
            index = end + 1;
        } else {
            break;
        }
    }
    out
}

/// What one pass of the handler holds beside the request: the names a file
/// upload protects (`PG(rfc1867_protected_variables)`), the arrays it
/// writes, whether a refusal is thrown (`request_parse_body()`) and the
/// throwable a refusal left pending, and what it keeps on the request's
/// heap from part to part to `fileupload_done`: the part's header lines,
/// `lbuf` with its room `llen`, `abuf` and `array_index`.
struct Handler {
    protected: Owner<PhpArray>,
    post: PhpArray,
    files: PhpArray,
    throwing: bool,
    pending: Option<Thrown>,
    headers: Headers,
    lbuf: Option<(Emalloc, usize)>,
    abuf: Option<Text>,
    array_index: Option<Text>,
}

impl Handler {
    /// What `rfc1867_post_handler` lays down before the first part: the
    /// protected names' table, `SG(rfc1867_uploaded_files)`
    /// ([`Request::uploaded_files`]) and `$_FILES`'s array where the
    /// request has none yet.
    fn new(req: &mut Request, post: PhpArray, throwing: bool) -> Result<Handler, Thrown> {
        let protected = PhpArray::new(req)?;
        let files = req.uploaded_files().and_then(|_| {
            if let Some(files) = req.files_track() {
                return Ok(files);
            }
            let files = req.array()?;
            req.set_files_track(files);
            Ok(files)
        });
        let files = match files {
            Ok(files) => files,
            Err(thrown) => {
                protected.give_up(req);
                return Err(thrown);
            }
        };
        Ok(Handler {
            protected,
            post,
            files,
            throwing,
            pending: None,
            headers: Headers::default(),
            lbuf: None,
            abuf: None,
            array_index: None,
        })
    }

    /// `EMIT_WARNING_OR_ERROR`: php's warning, or under
    /// `request_parse_body()` its `RequestParseBodyException`, thrown as
    /// `zend_throw_exception_ex` throws it, pending while the body is
    /// parsed on, so a later refusal is what leaves with the earlier one
    /// its `previous` (`Thrown::chain`).
    fn refuse(&mut self, req: &mut Request, message: &str) {
        if !self.throwing {
            docref(req, Level::Warning, message);
            return;
        }
        let now = throw(req, "RequestParseBodyException", message);
        self.pending = Some(match self.pending.take() {
            Some(earlier) => earlier.chain(req, now),
            None => now,
        });
    }

    /// What leaves the handler: what it served, and the throwable a
    /// refusal left pending, the one raised later chained over it.
    fn leaving(&mut self, req: &mut Request, served: Result<(), Thrown>) -> Result<(), Thrown> {
        match (self.pending.take(), served) {
            (None, served) => served,
            (Some(pending), Ok(())) => Err(pending),
            (Some(pending), Err(later)) => Err(pending.chain(req, later)),
        }
    }

    /// `add_protected_variable` at 8.5.12: the name, normalized, kept in
    /// the table (`zend_hash_str_add_empty_element`); `false` once the
    /// memory limit was met.
    fn protect(&mut self, req: &mut Request, name: &[u8]) -> Result<bool, Thrown> {
        let name = normalize_protected(name);
        self.protected.set_str(req, &name, Owner::null())?;
        Ok(!req.has_oom())
    }

    /// `safe_php_register_variable_ex` at 8.5.12: a protected name
    /// skipped unless `override_protection`; `false` once the memory
    /// limit was met.
    fn register(
        &self,
        req: &mut Request,
        into: PhpArray,
        name: &[u8],
        value: Owner<Value>,
        override_protection: bool,
    ) -> Result<bool, Thrown> {
        if !override_protection && self.protected.find(&normalize_protected(name)).is_some() {
            value.give_up(req);
        } else {
            req.register_in(into, name, value, false)?;
        }
        Ok(!req.has_oom())
    }

    /// A string value registered into `$_FILES`
    /// (`register_http_post_files_variable`), made as
    /// `php_register_variable_safe`'s `ZVAL_STRINGL_FAST` makes it
    /// ([`PhpString::fast`]): past one byte `zend_string_finit`'s ask,
    /// the bytes past the empty string's aligned header, so a name of
    /// five bytes takes the 40-byte bin where `zend_string_alloc` would
    /// take 32.
    fn file_string(
        &self,
        req: &mut Request,
        name: &[u8],
        value: &[u8],
        override_protection: bool,
    ) -> Result<bool, Thrown> {
        let value = PhpString::fast(req, value)?.into_value();
        self.register(req, self.files, name, value, override_protection)
    }

    /// `lbuf` with room for `param` and its longest index:
    /// `safe_ferealloc(lbuf, strlen(param), 1, MAX_SIZE_OF_INDEX + 1,
    /// true)` where it has less; `false` where the memory limit refused
    /// it.
    fn room_for(&mut self, req: &mut Request, param: &[u8]) -> Result<bool, Thrown> {
        if self
            .lbuf
            .as_ref()
            .is_some_and(|(_, llen)| *llen >= param.len() + INDEX_ROOM)
        {
            return Ok(true);
        }
        let size = param.len() + INDEX_ROOM;
        let (mut held, llen) = match self.lbuf.take() {
            Some((block, llen)) => (Some(block), llen),
            None => (None, 0),
        };
        let Some(grown) = grow(req, &mut held, size)? else {
            self.lbuf = held.map(|block| (block, llen));
            return Ok(false);
        };
        self.lbuf = Some((grown, size));
        Ok(true)
    }

    /// What `fileupload_done` frees: `lbuf`, `abuf`, `array_index`, the
    /// protected names' table and the header lines.
    fn free(self, req: &mut Request) {
        let Handler {
            protected,
            mut headers,
            lbuf,
            abuf,
            array_index,
            ..
        } = self;
        if let Some((block, _)) = lbuf {
            block.free(req);
        }
        free(req, abuf);
        free(req, array_index);
        protected.give_up(req);
        headers.clean(req);
    }
}

/// `rfc1867_post_handler` (`Z/main/rfc1867.c:640`) over the body, into
/// `post`, `$_POST`'s array, and the request's `$_FILES`: the limits and
/// the boundary, then the parts between the upload hook's start and end.
pub(crate) fn post_handler(
    req: &mut Request,
    content_type: &[u8],
    post: PhpArray,
    limits: &BodyLimits,
) -> Result<(), Thrown> {
    let upload_cnt = limits
        .max_file_uploads
        .unwrap_or_else(|| req.ini_int(b"max_file_uploads"));
    let mut body_parts_cnt = limits
        .max_multipart_body_parts
        .unwrap_or_else(|| req.ini_int(b"max_multipart_body_parts"));
    let post_max_size = req.post_max_size(limits);
    let max_input_vars = limits
        .max_input_vars
        .unwrap_or_else(|| req.ini().long(b"max_input_vars"));
    let upload_max_filesize = limits
        .upload_max_filesize
        .unwrap_or_else(|| req.ini().long(b"upload_max_filesize"));
    let throwing = req.post_throwing();
    let content_length = req.content_length();
    let refuse = |req: &mut Request, message: &str| {
        if throwing {
            return Err(throw(req, "RequestParseBodyException", message));
        }
        docref(req, Level::Warning, message);
        Ok(())
    };
    if post_max_size > 0 && content_length > post_max_size {
        return refuse(
            req,
            &format!(
                "POST Content-Length of {content_length} bytes exceeds the limit of {post_max_size} bytes"
            ),
        );
    }
    if body_parts_cnt < 0 {
        body_parts_cnt = max_input_vars.saturating_add(upload_cnt);
    }
    let body_parts_limit = body_parts_cnt;

    // The boundary.
    let lower = content_type.to_ascii_lowercase();
    let at = find(content_type, b"boundary").or_else(|| find(&lower, b"boundary"));
    let Some(eq) = at.and_then(|at| {
        content_type[at..]
            .iter()
            .position(|b| *b == b'=')
            .map(|eq| at + eq)
    }) else {
        return refuse(req, "Missing boundary in multipart/form-data POST data");
    };
    let mut boundary = &content_type[eq + 1..];
    if boundary.first() == Some(&b'"') {
        boundary = &boundary[1..];
        let Some(close) = boundary.iter().position(|b| *b == b'"') else {
            return refuse(req, "Invalid boundary in multipart/form-data POST data");
        };
        boundary = &boundary[..close];
    } else if let Some(end) = boundary.iter().position(|b| matches!(b, b',' | b';')) {
        boundary = &boundary[..end];
    }
    let boundary = c_str(boundary);
    if boundary.len() > FILLUNIT - 4 {
        req.note_error(
            Level::Warning,
            "Boundary too large in multipart/form-data POST data",
        );
        return Ok(());
    }
    let mut mbuff = Buffer::new(req, boundary, post_max_size)?;
    let mut upload = match Handler::new(req, post, throwing) {
        Ok(upload) => upload,
        Err(thrown) => {
            mbuff.free(req);
            return Err(thrown);
        }
    };
    let translation = Translation::of(req);
    let limits = Limits {
        upload_cnt,
        body_parts_cnt,
        body_parts_limit,
        max_input_vars,
        upload_max_filesize,
    };
    let served = (|| {
        let parsed = if req.upload_event(&Upload::Start { content_length })? && !req.has_oom() {
            parts(req, &mut upload, &mut mbuff, limits, translation.as_ref())
        } else {
            Ok(())
        };
        // A throwable a callback left is pending in php, which goes on to
        // `fileupload_done` all the same; a fatal or an exit leaves.
        if let Err(thrown) = &parsed
            && !matches!(thrown.cause(req), Cause::Throwable(_))
        {
            return parsed;
        }
        mbuff.warn_past_limit(req);
        parsed?;
        let read = req.post_bytes_read();
        req.upload_event(&Upload::End { read })?;
        Ok(())
    })();
    let served = upload.leaving(req, served);
    upload.free(req);
    mbuff.free(req);
    served
}

/// The limits one body is read under, as `rfc1867_post_handler` reads
/// them.
#[derive(Clone, Copy)]
struct Limits {
    upload_cnt: i64,
    body_parts_cnt: i64,
    body_parts_limit: i64,
    max_input_vars: i64,
    upload_max_filesize: i64,
}

/// What a part's `Content-Disposition` names: `name` and `filename`, each
/// as `getword_conf` copied it onto the request's heap.
#[derive(Default)]
struct Disposition {
    param: Option<Text>,
    filename: Option<Text>,
}

impl Disposition {
    /// `efree` of both, where php frees them on its way to
    /// `fileupload_done`.
    fn free(self, req: &mut Request) {
        free(req, self.filename);
        free(req, self.param);
    }
}

/// The words of a part's `Content-Disposition`, each pair and its key
/// copied by `getword`, freed as the next is read; `None` once the memory
/// limit was met (`Z/main/rfc1867.c:847-916` at 8.5.12).
fn disposition(
    req: &mut Request,
    mut cd: &[u8],
    step_in: &dyn Fn(u8) -> usize,
    mb: bool,
    convert: &dyn Fn(&mut Request, Text) -> Result<Text, Thrown>,
) -> Result<Option<Disposition>, Thrown> {
    let mut words = Disposition::default();
    cd = &cd[cd.iter().take_while(|b| is_space(**b)).count()..];
    while !cd.is_empty() {
        let pair = getword(&mut cd, b';', step_in);
        let word = Text::dup(req, &pair)?;
        if req.has_oom() {
            word.free(req);
            break;
        }
        cd = &cd[cd.iter().take_while(|b| is_space(**b)).count()..];
        let mut key = None;
        if pair.contains(&b'=') {
            let mut rest: &[u8] = &pair;
            let name = getword(&mut rest, b'=', step_in);
            let copied = Text::dup(req, &name)?;
            if req.has_oom() {
                word.free(req);
                copied.free(req);
                break;
            }
            let slot = if name.eq_ignore_ascii_case(b"name") {
                Some(&mut words.param)
            } else if name.eq_ignore_ascii_case(b"filename") {
                Some(&mut words.filename)
            } else {
                None
            };
            if let Some(slot) = slot {
                free(req, slot.take());
                let (value, asked) = getword_conf(rest, step_in);
                let size = asked.map_or(1, |len| len + 1 + usize::from(mb));
                let value = Text::sized(req, size, &value)?;
                *slot = Some(convert(req, value)?);
            }
            if req.has_oom() {
                word.free(req);
                copied.free(req);
                break;
            }
            key = Some(copied);
        }
        free(req, key);
        word.free(req);
    }
    if req.has_oom() {
        words.free(req);
        return Ok(None);
    }
    Ok(Some(words))
}

/// `rfc1867_post_handler`'s loop over the parts, to `fileupload_done`,
/// stopping at each of php 8.5.12's `ZEND_HAS_OOM()` once the memory
/// limit was met.
#[allow(clippy::too_many_lines)] // php's one loop, kept as one
fn parts(
    req: &mut Request,
    upload: &mut Handler,
    mbuff: &mut Buffer,
    limits: Limits,
    translation: Option<&Translation>,
) -> Result<(), Thrown> {
    let Limits {
        mut upload_cnt,
        mut body_parts_cnt,
        body_parts_limit,
        max_input_vars,
        upload_max_filesize,
    } = limits;
    let mut anonymous_index = 0u32;
    let mut count = 0i64;
    let mut max_file_size: i64 = 0;

    while !mbuff.eof(req) {
        upload.headers.clean(req);
        // A previous body part has reached the soft memory limit.
        if req.has_oom() {
            break;
        }
        if !mbuff.headers(req, &mut upload.headers)? {
            break;
        }
        let Some(cd) = upload
            .headers
            .find(b"Content-Disposition")
            .map(<[u8]>::to_vec)
        else {
            continue;
        };
        body_parts_cnt -= 1;
        if body_parts_cnt < 0 {
            upload.refuse(
                req,
                &format!(
                    "Multipart body parts limit exceeded {body_parts_limit}. To increase the limit change max_multipart_body_parts in php.ini."
                ),
            );
            break;
        }
        // Translated, the part is read in the encoding its last header
        // line is detected in, its words a character at a time where
        // there is an internal encoding to convert them to.
        let input = translation.and_then(|t| t.functions.detect(&mbuff.last, &t.http_input));
        let words = translation.filter(|t| t.internal.is_some());
        let step_in = |lead: u8| match (words, input) {
            (Some(t), Some(input)) => t.functions.char_len(input, lead),
            _ => 1,
        };
        let convert = |req: &mut Request, text: Text| match translation {
            Some(t) => t.convert(req, text, input),
            None => Ok(text),
        };
        let Some(Disposition { param, filename }) =
            disposition(req, &cd, &step_in, words.is_some(), &convert)?
        else {
            break;
        };

        // A form field, read whole.
        if filename.is_none()
            && let Some(param) = param
        {
            let value = mbuff.read_body(req)?;
            if req.has_oom() {
                free(req, value);
                param.free(req);
                break;
            }
            let value = match value {
                Some(value) => value,
                None => Text::dup(req, b"")?,
            };
            let value = convert(req, value)?;
            count += 1;
            let read = req.post_bytes_read();
            let kept = if count <= max_input_vars {
                req.filter_input(Input::Post, param.bytes(), value.bytes())?
            } else {
                None
            };
            let name = c_str(param.bytes()).to_vec();
            let registered = if let Some(kept) = kept {
                if req.has_oom() {
                    kept.give_up(req);
                    value.free(req);
                    param.free(req);
                    break;
                }
                let shown = kept.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default();
                let event = Upload::FormData {
                    read,
                    name: &name,
                    value: &shown,
                };
                match req.upload_event(&event) {
                    Ok(true) if req.has_oom() => {
                        kept.give_up(req);
                        false
                    }
                    Ok(true) => upload.register(req, upload.post, param.bytes(), kept, false)?,
                    Ok(false) => {
                        kept.give_up(req);
                        !req.has_oom()
                    }
                    Err(thrown) => {
                        kept.give_up(req);
                        value.free(req);
                        param.free(req);
                        return Err(thrown);
                    }
                }
            } else {
                if count == max_input_vars + 1 {
                    upload.refuse(
                        req,
                        &format!(
                            "Input variables exceeded {max_input_vars}. To increase the limit change max_input_vars in php.ini."
                        ),
                    );
                }
                let event = Upload::FormData {
                    read,
                    name: &name,
                    value: value.bytes(),
                };
                if let Err(thrown) = req.upload_event(&event) {
                    value.free(req);
                    param.free(req);
                    return Err(thrown);
                }
                !req.has_oom()
            };
            if !registered {
                value.free(req);
                param.free(req);
                break;
            }
            if name.eq_ignore_ascii_case(b"MAX_FILE_SIZE") {
                max_file_size = crate::c_number::strtol(value.bytes());
            }
            param.free(req);
            value.free(req);
            continue;
        }

        let mut skip_upload = false;
        if !req.ini_flag(b"file_uploads") {
            skip_upload = true;
        } else if upload_cnt <= 0 {
            skip_upload = true;
            if upload_cnt == 0 {
                upload_cnt -= 1;
                upload.refuse(
                    req,
                    "Maximum number of allowable file uploads has been exceeded",
                );
            }
        }
        let Some(filename) = filename else {
            upload.refuse(req, "File Upload Mime headers garbled");
            break;
        };
        let param_text = if let Some(param) = param {
            param
        } else {
            let name = anonymous_index.to_string().into_bytes();
            anonymous_index += 1;
            Text::sized(req, ANONYMOUS_NAME, &name)?
        };
        let param = c_str(param_text.bytes()).to_vec();
        let named = Disposition {
            param: Some(param_text),
            filename: Some(filename),
        };
        // Never repair potential malicious input: brackets that do not
        // nest, or text after a closing one, skip the upload.
        if !skip_upload {
            let mut depth = 0i64;
            for (at, byte) in param.iter().enumerate() {
                if *byte == b'[' {
                    depth += 1;
                } else if *byte == b']' {
                    depth -= 1;
                    if param.get(at + 1).is_some_and(|next| *next != b'[') {
                        skip_upload = true;
                        break;
                    }
                }
                if depth < 0 {
                    skip_upload = true;
                    break;
                }
            }
            if depth != 0 {
                skip_upload = true;
            }
        }
        let filename = (named.filename.as_ref())
            .map(|t| c_str(t.bytes()).to_vec())
            .unwrap_or_default();
        if !skip_upload {
            let event = Upload::FileStart {
                read: req.post_bytes_read(),
                name: &param,
                filename: &filename,
            };
            skip_upload = match req.upload_event(&event) {
                Ok(told) => !told,
                Err(thrown) => {
                    named.free(req);
                    return Err(thrown);
                }
            };
            if !skip_upload && req.has_oom() {
                named.free(req);
                break;
            }
        }
        if skip_upload {
            named.free(req);
            continue;
        }
        let mut cancel_upload = 0;
        if filename.is_empty() {
            cancel_upload = UPLOAD_ERROR_D;
        }
        let mut end = false;
        let mut total_bytes: i64 = 0;
        let mut temp: Option<(std::fs::File, std::path::PathBuf)> = None;
        let mut blen = Vec::new();
        if cancel_upload == 0 {
            blen = mbuff.read(req, FILLUNIT, &mut end);
            let dir = req.ini().get(b"upload_tmp_dir").map(<[u8]>::to_vec);
            temp = req.open_temporary_fd(dir.as_deref(), b"php", TemporaryGuard::OnFallback);
            upload_cnt -= 1;
            if temp.is_none() {
                upload.refuse(req, "File upload error - unable to create a temporary file");
                cancel_upload = UPLOAD_ERROR_E;
            }
        }
        let mut offset = 0i64;
        while cancel_upload == 0 && !blen.is_empty() {
            let size = i64::try_from(blen.len()).unwrap_or(i64::MAX);
            let event = Upload::FileData {
                read: req.post_bytes_read(),
                offset,
                length: size,
            };
            let told = match req.upload_event(&event) {
                Ok(told) => told,
                Err(thrown) => {
                    if let Some((_, path)) = &temp {
                        std::fs::remove_file(path).ok();
                    }
                    named.free(req);
                    return Err(thrown);
                }
            };
            if !told || req.has_oom() {
                cancel_upload = UPLOAD_ERROR_X;
                continue;
            }
            if upload_max_filesize > 0 && total_bytes + size > upload_max_filesize {
                cancel_upload = UPLOAD_ERROR_A;
            } else if max_file_size != 0 && total_bytes + size > max_file_size {
                cancel_upload = UPLOAD_ERROR_B;
            } else {
                use std::io::Write;
                let written = temp.as_mut().map(|(file, _)| file.write_all(&blen));
                match written {
                    Some(Ok(())) => total_bytes += size,
                    _ => cancel_upload = UPLOAD_ERROR_F,
                }
                offset += size;
            }
            blen = mbuff.read(req, FILLUNIT, &mut end);
        }
        let temp_path = temp.map(|(_, path)| path);
        if cancel_upload == 0 && !end {
            cancel_upload = UPLOAD_ERROR_C;
        }
        let shown = temp_path
            .as_ref()
            .map(|path| path.as_os_str().as_encoded_bytes().to_vec());
        let event = Upload::FileEnd {
            read: req.post_bytes_read(),
            temp: shown.as_deref(),
            error: cancel_upload,
        };
        let told = match req.upload_event(&event) {
            Ok(told) => told,
            Err(thrown) => {
                if let Some(path) = &temp_path {
                    std::fs::remove_file(path).ok();
                }
                named.free(req);
                return Err(thrown);
            }
        };
        if !told || req.has_oom() {
            cancel_upload = UPLOAD_ERROR_X;
        }
        let temp_name = if cancel_upload != 0 {
            if let Some(path) = &temp_path
                && cancel_upload != UPLOAD_ERROR_E
            {
                std::fs::remove_file(path).ok();
            }
            None
        } else {
            match temp_path {
                Some(path) => {
                    let bytes = path.into_os_string().into_encoded_bytes();
                    if let Err(thrown) = req.note_uploaded(&bytes) {
                        named.free(req);
                        return Err(thrown);
                    }
                    Some(bytes)
                }
                None => None,
            }
        };
        if req.has_oom() {
            named.free(req);
            break;
        }

        // `$foo[name]`, `$foo[full_path]`, `$foo[type]`, `$foo[tmp_name]`,
        // `$foo[error]` and `$foo[size]`, or `$foo[name][index]` and its
        // like where the field's name ends in an index.
        let array = param
            .iter()
            .position(|b| *b == b'[')
            .filter(|_| param.last() == Some(&b']'));
        if let Some(open) = array {
            free(req, upload.array_index.take());
            upload.array_index = Some(Text::dup(req, &param[open + 1..param.len() - 1])?);
            if req.has_oom() {
                named.free(req);
                break;
            }
        }
        if !upload.room_for(req, &param)? {
            named.free(req);
            break;
        }
        if let Some(open) = array {
            free(req, upload.abuf.take());
            upload.abuf = Some(Text::dup(req, &param[..open])?);
            if req.has_oom() {
                named.free(req);
                break;
            }
        }
        let slot = |field: &str| -> Vec<u8> {
            let (base, index) = array.map_or((param.as_slice(), None), |open| {
                (&param[..open], Some(&param[open + 1..param.len() - 1]))
            });
            let mut name = base.to_vec();
            name.push(b'[');
            name.extend_from_slice(field.as_bytes());
            name.push(b']');
            if let Some(index) = index {
                name.push(b'[');
                name.extend_from_slice(index);
                name.push(b']');
            }
            name
        };
        let step_internal = |lead: u8| match words_of(translation) {
            Some((t, internal)) => t.functions.char_len(internal, lead),
            None => 1,
        };
        let described = describe(
            req,
            upload,
            &param,
            &slot,
            basename(&filename, &step_internal),
            &filename,
            cancel_upload,
            temp_name.as_deref(),
            total_bytes,
        )?;
        named.free(req);
        if !described {
            break;
        }
    }
    Ok(())
}

/// The internal encoding a translated body's words step by, where it
/// has one.
fn words_of(translation: Option<&Translation>) -> Option<(&Translation, Encoding)> {
    translation.and_then(|t| t.internal.map(|internal| (t, internal)))
}

/// A file's six rows of `$_FILES` and the two names it protects, in
/// php's order; `false` once the memory limit was met at one, php's
/// `goto oom_with_param`.
#[allow(clippy::too_many_arguments)] // the locals php's loop holds
fn describe(
    req: &mut Request,
    upload: &mut Handler,
    param: &[u8],
    slot: &dyn Fn(&str) -> Vec<u8>,
    name: &[u8],
    filename: &[u8],
    cancel_upload: i64,
    temp_name: Option<&[u8]>,
    total_bytes: i64,
) -> Result<bool, Thrown> {
    if !upload.file_string(req, &slot("name"), name, false)?
        || !upload.file_string(req, &slot("full_path"), filename, false)?
    {
        return Ok(false);
    }
    let content_type = if cancel_upload != 0 {
        Vec::new()
    } else {
        (upload.headers.find(b"Content-Type"))
            .map(|value| {
                c_str(value)
                    .split(|b| *b == b';')
                    .next()
                    .unwrap_or_default()
                    .to_vec()
            })
            .unwrap_or_default()
    };
    if !upload.file_string(req, &slot("type"), &content_type, false)? {
        return Ok(false);
    }
    let tmp_slot = slot("tmp_name");
    if !upload.protect(req, param)? || !upload.protect(req, &tmp_slot)? {
        return Ok(false);
    }
    // The string the uploads' table keys the file by, shared, as
    // `ZVAL_STR_COPY(&zfilename, temp_filename)` shares it; the empty
    // string where the upload was cancelled.
    let shared = temp_name
        .and_then(|path| req.uploaded_name(path))
        .map(|name| Owner::take(req, name).into_value());
    let registered = match shared {
        Some(name) => upload.register(req, upload.files, &tmp_slot, name, true)?,
        None => upload.file_string(req, &tmp_slot, b"", true)?,
    };
    if !registered {
        return Ok(false);
    }
    let files = upload.files;
    if !upload.register(req, files, &slot("error"), Owner::int(cancel_upload), false)? {
        return Ok(false);
    }
    let size = if cancel_upload != 0 { 0 } else { total_bytes };
    upload.register(req, files, &slot("size"), Owner::int(size), false)
}

/// Where `needle` first stands in `haystack`, as `strstr` finds it.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let haystack = c_str(haystack);
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disposition_word_keeps_its_quotes_and_its_value_reads_them() {
        let mut line: &[u8] = b"form-data; name=\"a;b\"; filename='x\\'y'";
        let byte = |_: u8| 1;
        assert_eq!(getword(&mut line, b';', &byte), b"form-data");
        let rest = line.trim_ascii_start();
        let mut rest = rest;
        let pair = getword(&mut rest, b';', &byte);
        assert_eq!(pair, b"name=\"a;b\"");
        let mut value: &[u8] = &pair;
        assert_eq!(getword(&mut value, b'=', &byte), b"name");
        assert_eq!(getword_conf(value, &byte).0, b"a;b");
        assert_eq!(getword_conf(b" 'x\\'y'", &byte).0, b"x'y");
        assert_eq!(getword_conf(b"name\\\\5", &byte).0, b"name\\5");
        assert_eq!(basename(b"C:\\dir/sub\\f.txt", &byte), b"f.txt");
        // Shift_JIS's `\x95\x5c`, one character whose second byte is a
        // backslash: stepped over whole, it neither escapes nor splits.
        let sjis = |lead: u8| if lead >= 0x81 { 2 } else { 1 };
        assert_eq!(getword_conf(b"\"a\x95\\b\"", &sjis).0, b"a\x95\\b");
        assert_eq!(basename(b"d/\x95\\x", &sjis), b"\x95\\x");
        assert_eq!(normalize_protected(b" a.b[ x][y]"), b"a_b[x][y]");
    }

    #[test]
    fn a_boundary_prefix_at_the_end_is_where_a_read_stops() {
        assert_eq!(memstr(b"abc\n--xy", b"\n--xyz", true), Some(3));
        assert_eq!(memstr(b"abc\n--xy", b"\n--xyz", false), None);
        assert_eq!(memstr(b"abc", b"\n--x", true), None);
    }
}
