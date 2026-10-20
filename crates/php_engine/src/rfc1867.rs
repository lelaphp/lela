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

use crate::api::abi::{Encoding, Multibyte};
use crate::api::raising::{Level, Thrown, docref, throw};
use crate::api::request::temporary::TemporaryGuard;
use crate::api::request::{BodyLimits, Input, Request, Upload};
use crate::api::values::{Owner, PhpArray, PhpString, Value};
use crate::c_number::{c_str, is_space};

/// `FILLUNIT` (`Z/main/rfc1867.c:188`).
const FILLUNIT: usize = 1024 * 5;

/// `UPLOAD_ERR_*` (`Z/main/rfc1867.h:30-37`).
const UPLOAD_ERROR_A: i64 = 1;
const UPLOAD_ERROR_B: i64 = 2;
const UPLOAD_ERROR_C: i64 = 3;
const UPLOAD_ERROR_D: i64 = 4;
const UPLOAD_ERROR_E: i64 = 6;
const UPLOAD_ERROR_F: i64 = 7;
const UPLOAD_ERROR_X: i64 = 8;

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
}

impl Buffer {
    /// `multipart_buffer_new`.
    fn new(boundary: &[u8]) -> Buffer {
        let size = (boundary.len() + 6).max(FILLUNIT);
        let mut dashed = b"--".to_vec();
        dashed.extend_from_slice(boundary);
        let mut next = b"\n--".to_vec();
        next.extend_from_slice(boundary);
        Buffer {
            bytes: vec![0; size + 1],
            begin: 0,
            len: 0,
            size,
            boundary: dashed,
            boundary_next: next,
            last: Vec::new(),
        }
    }

    /// The bytes held.
    fn held(&self) -> &[u8] {
        &self.bytes[self.begin..self.begin + self.len]
    }

    /// `fill_buffer`: what is held moved to the start, and the free room
    /// filled from the host; how much came.
    fn fill(&mut self, req: &mut Request) -> usize {
        if self.len > 0 && self.begin != 0 {
            self.bytes.copy_within(self.begin..self.begin + self.len, 0);
        }
        self.begin = 0;
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

    /// `multipart_buffer_headers`: past the boundary, each header line
    /// up to the blank one, a line opening with white space or holding
    /// no colon added to the one before.
    fn headers(&mut self, req: &mut Request) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        if !self.find_boundary(req) {
            return None;
        }
        let mut headers = Vec::new();
        let mut current: Option<(Vec<u8>, Vec<u8>)> = None;
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
                if let Some(done) = current.take() {
                    headers.push(done);
                }
                let mut value = colon + 1;
                while line.get(value).is_some_and(|b| is_space(*b)) {
                    value += 1;
                }
                current = Some((
                    line[..colon].to_vec(),
                    line[value.min(line.len())..].to_vec(),
                ));
            } else if let Some((_, value)) = current.as_mut() {
                value.extend_from_slice(&line);
            }
        }
        if let Some(done) = current {
            headers.push(done);
        }
        Some(headers)
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

    /// `multipart_buffer_read_body`: what `read` hands out, to the end
    /// of the part.
    fn read_body(&mut self, req: &mut Request) -> Option<Vec<u8>> {
        let mut out: Option<Vec<u8>> = None;
        let mut end = false;
        loop {
            let chunk = self.read(req, FILLUNIT, &mut end);
            if chunk.is_empty() {
                break;
            }
            out.get_or_insert_with(Vec::new).extend_from_slice(&chunk);
        }
        out
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
/// taken for itself; `php_mb_rfc1867_getword_conf` under `step`.
fn getword_conf(s: &[u8], step: &dyn Fn(u8) -> usize) -> Vec<u8> {
    let start = s.iter().take_while(|b| is_space(**b)).count();
    let s = &s[start..];
    let Some(&first) = s.first() else {
        return Vec::new();
    };
    if first == b'"' || first == b'\'' {
        substring_conf(&s[1..], first, step)
    } else {
        let end = s.iter().position(|b| is_space(*b)).unwrap_or(s.len());
        substring_conf(&s[..end], 0, step)
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
    /// stand and the conversion answers.
    fn convert(&self, text: Vec<u8>, input: Option<Encoding>) -> Vec<u8> {
        match (input, self.internal) {
            (Some(input), Some(internal)) => self
                .functions
                .convert(&text, internal, input)
                .unwrap_or(text),
            _ => text,
        }
    }
}

/// `normalize_protected_variable`: the name less its leading spaces, a
/// space or dot before the first `[` read as `_`, and each index's
/// leading white space taken out.
fn normalize_protected(name: &[u8]) -> Vec<u8> {
    let name = &name[name.iter().take_while(|b| **b == b' ').count()..];
    let mut out: Vec<u8> = Vec::new();
    let Some(open) = name.iter().position(|b| *b == b'[') else {
        return name
            .iter()
            .map(|b| if matches!(b, b' ' | b'.') { b'_' } else { *b })
            .collect();
    };
    out.extend(
        name[..open]
            .iter()
            .map(|b| if matches!(b, b' ' | b'.') { b'_' } else { *b }),
    );
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
/// writes, and whether a refusal is thrown (`request_parse_body()`).
struct Handler {
    protected: Vec<Vec<u8>>,
    post: PhpArray,
    files: PhpArray,
    throwing: bool,
}

impl Handler {
    /// `EMIT_WARNING_OR_ERROR`: php's `RequestParseBodyException` under
    /// `request_parse_body()`, the warning otherwise.
    fn refuse(&self, req: &mut Request, message: &str) -> Result<(), Thrown> {
        if self.throwing {
            return Err(throw(req, "RequestParseBodyException", message));
        }
        docref(req, Level::Warning, message);
        Ok(())
    }

    /// `safe_php_register_variable_ex`: a protected name skipped unless
    /// `override_protection`.
    fn register(
        &self,
        req: &mut Request,
        into: PhpArray,
        name: &[u8],
        value: Owner<Value>,
        override_protection: bool,
    ) -> Result<(), Thrown> {
        if !override_protection && self.protected.contains(&normalize_protected(name)) {
            value.give_up(req);
            return Ok(());
        }
        req.register_in(into, name, value, false)
    }

    /// A string value registered into `$_FILES`.
    fn file_string(
        &self,
        req: &mut Request,
        name: &[u8],
        value: &[u8],
        override_protection: bool,
    ) -> Result<(), Thrown> {
        let value = PhpString::new(req, value)?.into_value();
        self.register(req, self.files, name, value, override_protection)
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
    let files = if let Some(files) = req.files_track() {
        files
    } else {
        let files = req.array()?;
        req.set_files_track(files);
        files
    };
    let mut upload = Handler {
        protected: Vec::new(),
        post,
        files,
        throwing,
    };
    if post_max_size > 0 && content_length > post_max_size {
        return upload.refuse(
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
        return upload.refuse(req, "Missing boundary in multipart/form-data POST data");
    };
    let mut boundary = &content_type[eq + 1..];
    if boundary.first() == Some(&b'"') {
        boundary = &boundary[1..];
        let Some(close) = boundary.iter().position(|b| *b == b'"') else {
            return upload.refuse(req, "Invalid boundary in multipart/form-data POST data");
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
    let mut mbuff = Buffer::new(boundary);
    let translation = Translation::of(req);
    let limits = Limits {
        upload_cnt,
        body_parts_cnt,
        body_parts_limit,
        max_input_vars,
        upload_max_filesize,
    };
    if req.upload_event(&Upload::Start { content_length })? {
        parts(req, &mut upload, &mut mbuff, limits, translation.as_ref())?;
    }
    let read = req.post_bytes_read();
    req.upload_event(&Upload::End { read })?;
    Ok(())
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

/// `rfc1867_post_handler`'s loop over the parts, to `fileupload_done`.
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
        let Some(header) = mbuff.headers(req) else {
            break;
        };
        let find_header = |name: &[u8]| {
            header
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        let Some(cd) = find_header(b"Content-Disposition") else {
            continue;
        };
        body_parts_cnt -= 1;
        if body_parts_cnt < 0 {
            upload.refuse(
                req,
                &format!(
                    "Multipart body parts limit exceeded {body_parts_limit}. To increase the limit change max_multipart_body_parts in php.ini."
                ),
            )?;
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
        let convert = |text: Vec<u8>| match translation {
            Some(t) => t.convert(text, input),
            None => text,
        };
        let mut cd: &[u8] = &cd[cd.iter().take_while(|b| is_space(**b)).count()..];
        let mut param: Option<Vec<u8>> = None;
        let mut filename: Option<Vec<u8>> = None;
        while !cd.is_empty() {
            let pair = getword(&mut cd, b';', &step_in);
            cd = &cd[cd.iter().take_while(|b| is_space(**b)).count()..];
            if pair.contains(&b'=') {
                let mut rest: &[u8] = &pair;
                let key = getword(&mut rest, b'=', &step_in);
                if key.eq_ignore_ascii_case(b"name") {
                    param = Some(convert(getword_conf(rest, &step_in)));
                } else if key.eq_ignore_ascii_case(b"filename") {
                    filename = Some(convert(getword_conf(rest, &step_in)));
                }
            }
        }

        // A form field, read whole.
        if filename.is_none()
            && let Some(param) = param
        {
            let value = convert(mbuff.read_body(req).unwrap_or_default());
            count += 1;
            let read = req.post_bytes_read();
            let kept = if count <= max_input_vars {
                req.filter_input(Input::Post, &param, &value)?
            } else {
                None
            };
            if let Some(kept) = kept {
                let shown = kept.as_str_bytes().map(<[u8]>::to_vec).unwrap_or_default();
                let event = Upload::FormData {
                    read,
                    name: c_str(&param),
                    value: &shown,
                };
                match req.upload_event(&event) {
                    Ok(true) => upload.register(req, upload.post, &param, kept, false)?,
                    Ok(false) => kept.give_up(req),
                    Err(thrown) => {
                        kept.give_up(req);
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
                    )?;
                }
                let event = Upload::FormData {
                    read,
                    name: c_str(&param),
                    value: &value,
                };
                req.upload_event(&event)?;
            }
            if c_str(&param).eq_ignore_ascii_case(b"MAX_FILE_SIZE") {
                max_file_size = crate::c_number::strtol(&value);
            }
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
                )?;
            }
        }
        if param.is_none() && filename.is_none() {
            upload.refuse(req, "File Upload Mime headers garbled")?;
            break;
        }
        let param = param.unwrap_or_else(|| {
            let name = anonymous_index.to_string().into_bytes();
            anonymous_index += 1;
            name
        });
        let param = c_str(&param).to_vec();
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
        let filename = c_str(&filename.unwrap_or_default()).to_vec();
        if !skip_upload {
            let event = Upload::FileStart {
                read: req.post_bytes_read(),
                name: &param,
                filename: &filename,
            };
            skip_upload = !req.upload_event(&event)?;
        }
        if skip_upload {
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
                upload.refuse(req, "File upload error - unable to create a temporary file")?;
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
            if !req.upload_event(&event)? {
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
        if !req.upload_event(&event)? {
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
            temp_path.inspect(|path| req.note_uploaded(path.clone()))
        };

        // `$foo[name]`, `$foo[full_path]`, `$foo[type]`, `$foo[tmp_name]`,
        // `$foo[error]` and `$foo[size]`, or `$foo[name][index]` and its
        // like where the field's name ends in an index.
        let array = param
            .iter()
            .position(|b| *b == b'[')
            .filter(|_| param.last() == Some(&b']'));
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
        let step_internal = |lead: u8| match words.zip(words.and_then(|t| t.internal)) {
            Some((t, internal)) => t.functions.char_len(internal, lead),
            None => 1,
        };
        upload.file_string(
            req,
            &slot("name"),
            basename(&filename, &step_internal),
            false,
        )?;
        upload.file_string(req, &slot("full_path"), &filename, false)?;
        let content_type = if cancel_upload != 0 {
            Vec::new()
        } else {
            find_header(b"Content-Type")
                .map(|value| {
                    c_str(&value)
                        .split(|b| *b == b';')
                        .next()
                        .unwrap_or_default()
                        .to_vec()
                })
                .unwrap_or_default()
        };
        upload.file_string(req, &slot("type"), &content_type, false)?;
        upload.protected.push(normalize_protected(&param));
        let tmp_slot = slot("tmp_name");
        upload.protected.push(normalize_protected(&tmp_slot));
        let kept = temp_name
            .map(|path| path.into_os_string().into_encoded_bytes())
            .unwrap_or_default();
        upload.file_string(req, &tmp_slot, &kept, true)?;
        upload.register(
            req,
            upload.files,
            &slot("error"),
            Owner::int(cancel_upload),
            false,
        )?;
        let size = if cancel_upload != 0 { 0 } else { total_bytes };
        upload.register(req, upload.files, &slot("size"), Owner::int(size), false)?;
    }
    Ok(())
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
        assert_eq!(getword_conf(value, &byte), b"a;b");
        assert_eq!(getword_conf(b" 'x\\'y'", &byte), b"x'y");
        assert_eq!(getword_conf(b"name\\\\5", &byte), b"name\\5");
        assert_eq!(basename(b"C:\\dir/sub\\f.txt", &byte), b"f.txt");
        // Shift_JIS's `\x95\x5c`, one character whose second byte is a
        // backslash: stepped over whole, it neither escapes nor splits.
        let sjis = |lead: u8| if lead >= 0x81 { 2 } else { 1 };
        assert_eq!(getword_conf(b"\"a\x95\\b\"", &sjis), b"a\x95\\b");
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
