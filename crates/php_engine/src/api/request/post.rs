//! The body of a request of the web, `main/SAPI.c`'s post half and
//! `main/php_content_types.c`: at request start the content type picks
//! a reader and a handler (`sapi_read_post_data`), the form reader
//! buffers the body as `SG(request_info).request_body`
//! (`sapi_read_standard_form_data`), and when `$_POST` is made its handler
//! registers what the body holds (`sapi_handle_post`): a form through
//! `php_std_post_handler`, multipart through `rfc1867_post_handler`
//! ([`crate::rfc1867`]). The body is the description's
//! [`WebRequest::body`](crate::engine::WebRequest), a reader the request
//! pulls from as php's `sapi_module.read_post` reads stdin, block by
//! block as it is asked, so `php://input` reads what no reader took, as
//! php's does, and a body nothing asks for is never read.

use std::io::Read;
use std::path::PathBuf;

use super::Request;
use super::Variables;
use super::variables::BodyLimits;
use crate::api::abi::{Beneath, chain};
use crate::api::raising::{Level, Thrown, docref, throw_error};
use crate::api::request::temporary::TemporaryGuard;
use crate::api::values::{Key, Owner, PhpArray, PhpString, Value, adopted};
use crate::engine::{Body, Method};

/// `SAPI_POST_BLOCK_SIZE` (`Z/main/SAPI.h:28`): what the form reader
/// asks the host for at a time.
const POST_BLOCK_SIZE: usize = 0x4000;

/// The reader and handler a content type registered, one row of
/// `php_post_entries` (`Z/main/php_content_types.c:24`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostEntry {
    /// `application/x-www-form-urlencoded`: `sapi_read_standard_form_data`
    /// and `php_std_post_handler`.
    Form,
    /// `multipart/form-data`: no reader, and `rfc1867_post_handler`.
    Multipart,
}

impl PostEntry {
    /// The row a content type names: the type lowered and cut at its
    /// first `;`, `,` or space, as `sapi_read_post_data` cuts it.
    fn of(content_type: &[u8]) -> Option<PostEntry> {
        let named = content_type[..type_end(content_type)].to_ascii_lowercase();
        match named.as_slice() {
            b"application/x-www-form-urlencoded" => Some(PostEntry::Form),
            b"multipart/form-data" => Some(PostEntry::Multipart),
            _ => None,
        }
    }
}

/// One step of a multipart body's reading, as `rfc1867_post_handler` hands
/// it to `php_rfc1867_callback` (`MULTIPART_EVENT_*`, `Z/main/rfc1867.h`),
/// each with how much of the body the host has handed out by then,
/// `SG(read_post_bytes)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upload<'a> {
    /// `MULTIPART_EVENT_START`: the body's reading begins.
    Start { content_length: i64 },
    /// `MULTIPART_EVENT_FORMDATA`: a form field, its name and the value the
    /// input filter kept.
    FormData {
        read: i64,
        name: &'a [u8],
        value: &'a [u8],
    },
    /// `MULTIPART_EVENT_FILE_START`: a file's field and its name.
    FileStart {
        read: i64,
        name: &'a [u8],
        filename: &'a [u8],
    },
    /// `MULTIPART_EVENT_FILE_DATA`: a block of the file, at `offset`.
    FileData { read: i64, offset: i64, length: i64 },
    /// `MULTIPART_EVENT_FILE_END`: the file written, where it was, and its
    /// `UPLOAD_ERR_*`.
    FileEnd {
        read: i64,
        temp: Option<&'a [u8]>,
        error: i64,
    },
    /// `MULTIPART_EVENT_END`: the body's reading ends.
    End { read: i64 },
}

/// php's post state of one request (`SG(request_info)`, `SG(read_post_bytes)`,
/// `SG(post_read)`, `SG(rfc1867_uploaded_files)`).
#[derive(Debug, Default)]
pub(crate) struct Post {
    /// `SG(request_info).post_entry`.
    entry: Option<PostEntry>,
    /// `SG(request_info).content_type_dup`: the content type with what
    /// came before its first `;`, `,` or space lowered.
    content_type: Option<Vec<u8>>,
    /// `SG(read_post_bytes)`: how much of the body the host has handed
    /// out.
    read: usize,
    /// `SG(post_read)`: the host has nothing more to hand.
    done: bool,
    /// `SG(request_info).request_body`: what the form reader buffered,
    /// and what `php://input` read after it.
    request_body: Option<Vec<u8>>,
    /// `SG(rfc1867_uploaded_files)`: the table the multipart handler lays
    /// on the request's heap, `ALLOC_HASHTABLE` and `zend_hash_init`, each
    /// file an upload wrote a row of it until it is moved or the request
    /// ends, keyed by its temporary path, the one string `$_FILES` names
    /// it by too ([`Request::note_uploaded`]).
    pub(crate) uploaded: Option<PhpArray>,
    /// `SG(request_parse_body_context).throw_exceptions`: what the
    /// multipart handler refuses is thrown and not warned of, under
    /// `request_parse_body()`.
    pub(crate) throwing: bool,
}

impl Request {
    /// `sapi_activate`'s read of the body (`Z/main/SAPI.c:462`): where
    /// `enable_post_data_reading` is on and the host described a content
    /// type and the method `POST`, spelled so, the content type's reader.
    pub(crate) fn read_post_at_startup(&mut self) {
        let posting = self
            .web()
            .is_some_and(|web| web.content_type.is_some() && web.method == Some(Method::Post));
        if posting && self.ini_flag(b"enable_post_data_reading") {
            self.read_post_data(&BodyLimits::default());
        }
    }

    /// `sapi_read_post_data` (`Z/main/SAPI.c:170`): the reader the content
    /// type registered, and the default reader after it, which reads the
    /// body as a form's where no row was found.
    fn read_post_data(&mut self, limits: &BodyLimits) {
        let content_type = self
            .web()
            .and_then(|web| web.content_type.clone())
            .unwrap_or_default();
        let end = type_end(&content_type);
        let mut dup = content_type[..end].to_ascii_lowercase();
        dup.extend_from_slice(&content_type[end..]);
        let entry = PostEntry::of(&content_type);
        self.post.entry = entry;
        self.post.content_type = Some(dup);
        if entry == Some(PostEntry::Form) {
            self.read_standard_form_data(limits);
        }
        // `php_default_post_reader`: a body no row reads is swallowed.
        let post = self
            .web()
            .is_some_and(|web| web.method == Some(Method::Post));
        if post && entry.is_none() {
            self.read_standard_form_data(limits);
        }
    }

    /// `post_max_size`, or what `request_parse_body()` was handed.
    pub(crate) fn post_max_size(&self, limits: &BodyLimits) -> i64 {
        limits
            .post_max_size
            .unwrap_or_else(|| self.ini().long(b"post_max_size"))
    }

    /// `SG(request_info).content_length`.
    pub(crate) fn content_length(&self) -> i64 {
        self.web().map_or(0, |web| web.content_length)
    }

    /// `sapi_read_standard_form_data` (`Z/main/SAPI.c:254`): a body longer
    /// than `post_max_size` says so and is not read; one that is read is
    /// the request's body, block by block, ending with the warning where
    /// more came than the limit.
    fn read_standard_form_data(&mut self, limits: &BodyLimits) {
        let post_max_size = self.post_max_size(limits);
        let content_length = self.content_length();
        if post_max_size > 0 && content_length > post_max_size {
            docref(
                self,
                Level::Warning,
                &format!(
                    "POST Content-Length of {content_length} bytes exceeds the limit of {post_max_size} bytes"
                ),
            );
            return;
        }
        let mut body = Vec::new();
        let mut spilled = false;
        loop {
            let block = self.read_post_block(POST_BLOCK_SIZE);
            // `php_stream_temp_write`: the body's memory stream holds fewer
            // than `SAPI_POST_BLOCK_SIZE` bytes, and the write that would
            // reach it moves it to a temporary file in `upload_tmp_dir`;
            // one that cannot be made loses the whole body.
            if !spilled && !block.is_empty() && body.len() + block.len() >= POST_BLOCK_SIZE {
                let dir = self.ini().get(b"upload_tmp_dir").map(<[u8]>::to_vec);
                let Some((_, path)) =
                    self.open_temporary_fd(dir.as_deref(), b"php", TemporaryGuard::Never)
                else {
                    docref(
                        self,
                        Level::Warning,
                        "Unable to create temporary file, Check permissions in temporary files directory.",
                    );
                    docref(
                        self,
                        Level::Warning,
                        "POST data can't be buffered; all data discarded",
                    );
                    body.clear();
                    break;
                };
                spilled = true;
                std::fs::remove_file(path).ok();
            }
            body.extend_from_slice(&block);
            if post_max_size > 0
                && i64::try_from(self.post.read).unwrap_or(i64::MAX) > post_max_size
            {
                docref(
                    self,
                    Level::Warning,
                    &format!(
                        "Actual POST length does not match Content-Length, and exceeds {post_max_size} bytes"
                    ),
                );
                break;
            }
            if block.len() < POST_BLOCK_SIZE {
                break;
            }
        }
        self.post.request_body = Some(body);
    }

    /// `sapi_read_post_block` (`Z/main/SAPI.c:232`): up to `max` bytes of
    /// the body the host has not handed out, and the body counted read
    /// once it hands out fewer.
    pub(crate) fn read_post_block(&mut self, max: usize) -> Vec<u8> {
        let block = self.read_post(max);
        if block.len() < max {
            self.post.done = true;
        }
        block
    }

    /// `sapi_module.read_post`: up to `max` bytes of the body, pulled from
    /// the host's reader until `max` came or the reader ended, as php's
    /// hosts fill the buffer they are handed (`sapi_cgi_read_post`,
    /// `Z/sapi/cgi/cgi_main.c:477`), `SG(read_post_bytes)` moved on; a
    /// read that fails ends the body there, as their `read` answering
    /// `-1` does. Nothing, and the host's warning raised, where it could
    /// not read the body.
    pub(crate) fn read_post(&mut self, max: usize) -> Vec<u8> {
        let reader = match self.description.web.as_mut().map(|web| &mut web.body) {
            Some(Body::Stream(reader)) => reader,
            Some(Body::Refused(warning)) => {
                let warning = warning.clone();
                crate::api::raising::warning(self, &warning);
                return Vec::new();
            }
            None => return Vec::new(),
        };
        let mut block = vec![0; max];
        let mut filled = 0;
        while filled < max {
            match reader.read(&mut block[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        block.truncate(filled);
        self.post.read = self.post.read.saturating_add(filled);
        block
    }

    /// `sapi_handle_post` (`Z/main/SAPI.c:161`): the handler the content
    /// type registered over the body, into `array`, `$_POST`'s.
    pub(crate) fn handle_post(
        &mut self,
        array: PhpArray,
        limits: &BodyLimits,
    ) -> Result<(), Thrown> {
        let Some(content_type) = self.post.content_type.take() else {
            return Ok(());
        };
        let handled = match self.post.entry {
            Some(PostEntry::Form) => {
                let body = self.post.request_body.clone().unwrap_or_default();
                self.add_post_vars(array, &body, limits)
            }
            Some(PostEntry::Multipart) => {
                crate::rfc1867::post_handler(self, &content_type, array, limits)
            }
            None => return Ok(()),
        };
        // `if (ZEND_HAS_OOM()) zend_handle_delayed_oom();` (`Z/main/SAPI.c:168`
        // at 8.5.12): a body that took the request past its memory limit
        // ends it in the limit's fatal before the script runs.
        if self.has_oom() {
            self.handle_delayed_oom();
        }
        handled
    }

    /// What `php://input` reads (`Z/ext/standard/php_fopen_wrapper.c:77`):
    /// the body the form reader buffered, with what the host has not
    /// handed out read after it, which becomes the request's body; empty
    /// where a reader took the body and kept none of it, as multipart
    /// does.
    ///
    /// Read as `php_stream_input_read` reads, a chunk at a time: the host
    /// is asked for more only while what it has handed out falls short of
    /// the chunk wanted, so a body the form reader stopped reading part way
    /// is read no further.
    pub(crate) fn input_bytes(&mut self) -> Vec<u8> {
        const CHUNK: usize = 8192;
        let mut body = self.post.request_body.take().unwrap_or_default();
        let mut out = Vec::new();
        loop {
            let position = out.len();
            if !self.post.done && self.post.read < position + CHUNK {
                let block = self.read_post_block(CHUNK);
                body.extend_from_slice(&block);
            }
            let end = (position + CHUNK).min(body.len());
            if end <= position {
                break;
            }
            out.extend_from_slice(&body[position..end]);
        }
        self.post.request_body = Some(body);
        out
    }

    /// What [`parse_body`](Self::parse_body) makes, the two owners not
    /// yet named.
    fn parse_body_made(&mut self, limits: &BodyLimits) -> Result<(PhpArray, PhpArray), Thrown> {
        let Some(content_type) = self.web().and_then(|web| web.content_type.clone()) else {
            return Err(throw_error(
                self,
                "RequestParseBodyException",
                "Request does not provide a content type",
            ));
        };
        self.read_post_data(limits);
        if self.post.entry.is_none() {
            let text = format!(
                "Content-Type \"{}\" is not supported",
                String::from_utf8_lossy(&content_type)
            );
            return Err(throw_error(self, "RequestParseBodyException", &text));
        }
        let post = self.array()?;
        let files = self.array()?;
        let kept = self.http_globals.files.replace(files);
        let throwing = std::mem::replace(&mut self.post.throwing, true);
        let handled = self.handle_post(post, limits);
        self.post.throwing = throwing;
        let files = std::mem::replace(&mut self.http_globals.files, kept).unwrap_or(files);
        let thrown = match handled {
            Ok(()) if !self.is_pending() => return Ok((post, files)),
            Ok(()) => Thrown::of(None),
            Err(thrown) => thrown,
        };
        self.release(crate::api::values::Value::array(post));
        self.release(crate::api::values::Value::array(files));
        Err(thrown)
    }

    /// `destroy_uploaded_files_hash` (`Z/main/rfc1867.c:170`), at
    /// `sapi_deactivate_destroy`: every uploaded file no
    /// `move_uploaded_file()` moved, removed.
    pub(crate) fn destroy_uploaded_files(&mut self) {
        let Some(table) = self.post.uploaded.take() else {
            return;
        };
        let mut position = 0;
        while let Some((at, key, _)) = table.live_from(position) {
            if let Key::Str(path) = key {
                std::fs::remove_file(path_of(path.as_bytes())).ok();
            }
            position = at + 1;
        }
        self.release(Value::array(table));
    }

    /// `SG(rfc1867_uploaded_files)` as `rfc1867_post_handler` lays it
    /// down before it reads a part (`Z/main/rfc1867.c:795`): the table on
    /// the request's heap, laid once. php lays a fresh one each time the
    /// handler parses a body and forgets the last, its files with it; a
    /// second parse in one request keeps the first table here, so every
    /// file an upload wrote is still removed at the request's end.
    pub(crate) fn uploaded_files(&mut self) -> Result<PhpArray, Thrown> {
        if let Some(table) = self.post.uploaded {
            return Ok(table);
        }
        let table = self.array()?;
        self.post.uploaded = Some(table);
        Ok(table)
    }

    /// The string the table keys the upload at `path` by, the one
    /// `$_FILES` names it by, as `ZVAL_STR_COPY(&zfilename,
    /// temp_filename)` shares it; a view.
    pub(crate) fn uploaded_name(&self, path: &[u8]) -> Option<PhpString> {
        let table = self.post.uploaded?;
        let mut position = 0;
        while let Some((at, key, _)) = table.live_from(position) {
            if let Key::Str(name) = key
                && name.as_bytes() == path
            {
                return Some(name);
            }
            position = at + 1;
        }
        None
    }

    /// Whether a multipart refusal is thrown, under `request_parse_body()`.
    pub(crate) fn post_throwing(&self) -> bool {
        self.post.throwing
    }

    /// `PG(http_globals)[TRACK_VARS_FILES]`, where it is made.
    pub(crate) fn files_track(&self) -> Option<PhpArray> {
        self.http_globals.files
    }

    /// `PG(http_globals)[TRACK_VARS_FILES]` laid down as an upload makes it.
    pub(crate) fn set_files_track(&mut self, files: PhpArray) {
        self.http_globals.files = Some(files);
    }

    /// One file an upload wrote, kept for `is_uploaded_file()`:
    /// `zend_hash_add_ptr(SG(rfc1867_uploaded_files), temp_filename, ...)`,
    /// its temporary path a string on the request's heap and the key of
    /// its row.
    pub(crate) fn note_uploaded(&mut self, path: &[u8]) -> Result<(), Thrown> {
        let mut table = self.uploaded_files()?;
        let name = PhpString::new(self, path)?;
        table.set(self, name.into_key(), Owner::null())
    }

    /// `php_rfc1867_callback` over one step: the chain entered from the
    /// top ([`chain::Upload`]); `false` where it answered
    /// php's `FAILURE`.
    pub(crate) fn upload_event(&mut self, event: &Upload<'_>) -> Result<bool, Thrown> {
        let shared = std::sync::Arc::clone(&self.shared);
        Beneath::<chain::Upload>::top(shared.supplied.upload.links(), *event).serve(self)
    }

    /// How much of the body the host has handed out,
    /// `SG(read_post_bytes)`.
    pub(crate) fn post_bytes_read(&self) -> i64 {
        i64::try_from(self.post.read).unwrap_or(i64::MAX)
    }

    /// What [`input_variables`](Self::input_variables) makes, its one owner not yet named.
    pub(crate) fn input_variables_made(&mut self, input: super::Input) -> Result<PhpArray, Thrown> {
        let source = match input {
            super::Input::Get => self.web().and_then(|web| web.query.clone()),
            super::Input::Cookie => self.web().and_then(|web| web.cookies.clone()),
            _ => None,
        };
        self.treat_data(input, source.as_deref())
            .map(Owner::hand_on)
    }
}

impl Variables<&Request> {
    /// Whether `path` is a file an upload of this request wrote and no
    /// `move_uploaded_file()` has moved, `SG(rfc1867_uploaded_files)`.
    #[must_use]
    pub fn is_uploaded_file(self, path: &[u8]) -> bool {
        self.0
            .post
            .uploaded
            .is_some_and(|table| table.find(path).is_some())
    }
}

impl Variables<&mut Request> {
    /// `request_parse_body()`'s body (`Z/ext/standard/http.c:345`): the
    /// content type's reader over the body, under `limits` where the
    /// caller named them, and its handler into two fresh arrays, the
    /// `$_POST` and `$_FILES` it would have made, which the request's own
    /// are not; a request with no content type, or one no handler reads,
    /// is php's `RequestParseBodyException`.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn parse_body(
        self,
        limits: &BodyLimits,
    ) -> Result<(Owner<PhpArray>, Owner<PhpArray>), Thrown> {
        let (post, files) = self.0.parse_body_made(limits)?;
        Ok((Owner::adopt(post), Owner::adopt(files)))
    }

    /// `move_uploaded_file()`'s first half: take the upload's temporary
    /// path out of the request, once.
    pub fn take_uploaded_file(self, path: &[u8]) -> Option<PathBuf> {
        let name = self.0.uploaded_name(path)?;
        let mut table = self.0.post.uploaded?;
        table.unset(self.0, &Key::Str(name)).ok()?;
        Some(path_of(path))
    }

    /// The variables of the query string or the cookie header, parsed
    /// afresh by `php_default_treat_data` into an array of their own, as
    /// `session.upload_progress` asks `sapi_module.treat_data` for them
    /// before `$_GET` and `$_COOKIE` are made; empty for any other input.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn input_variables(self, input: super::Input) -> Result<Owner<PhpArray>, Thrown> {
        adopted(self.0.input_variables_made(input))
    }
}

/// Where a content type's type ends: its first `;`, `,` or space, as
/// `sapi_read_post_data` cuts it.
fn type_end(content_type: &[u8]) -> usize {
    (content_type.iter())
        .position(|b| matches!(b, b';' | b',' | b' '))
        .unwrap_or(content_type.len())
}

/// The path a temporary file's name is, its bytes as the system's.
fn path_of(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}
