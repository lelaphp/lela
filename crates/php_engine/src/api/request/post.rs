//! The body of a request of the web, `main/SAPI.c`'s post half and
//! `main/php_content_types.c`: at request start the content type picks
//! a reader and a handler (`sapi_read_post_data`), the form reader
//! buffers the body as `SG(request_info).request_body`
//! (`sapi_read_standard_form_data`), and when `$_POST` is made its handler
//! registers what the body holds (`sapi_handle_post`): a form through
//! `php_std_post_handler`, multipart through `rfc1867_post_handler`
//! ([`crate::rfc1867`]). What the host read is the description's
//! [`WebRequest::body`](crate::engine::WebRequest), handed out as php's
//! `sapi_module.read_post` hands out stdin, so `php://input` reads what no
//! reader took, as php's does.

use std::path::PathBuf;

use super::Request;
use super::variables::BodyLimits;
use crate::api::raising::{Level, Thrown, docref, throw};
use crate::api::values::PhpArray;
use crate::engine::Method;

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
        let end = content_type
            .iter()
            .position(|b| matches!(b, b';' | b',' | b' '))
            .unwrap_or(content_type.len());
        let named = content_type[..end].to_ascii_lowercase();
        match named.as_slice() {
            b"application/x-www-form-urlencoded" => Some(PostEntry::Form),
            b"multipart/form-data" => Some(PostEntry::Multipart),
            _ => None,
        }
    }
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
    /// `SG(rfc1867_uploaded_files)`: each file an upload wrote, by its
    /// temporary path, until it is moved or the request ends.
    pub(crate) uploaded: Vec<PathBuf>,
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
        if posting && self.ini_long(b"enable_post_data_reading") != 0 {
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
        let end = content_type
            .iter()
            .position(|b| matches!(b, b';' | b',' | b' '))
            .unwrap_or(content_type.len());
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
        limits.post_max_size.unwrap_or_else(|| {
            crate::ini_quantity::parse(self.ini_get(b"post_max_size").unwrap_or_default()).0
        })
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
        loop {
            let block = self.read_post_block(POST_BLOCK_SIZE);
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

    /// `sapi_module.read_post`: up to `max` bytes of the body the host
    /// read, past what it has handed out, `SG(read_post_bytes)` moved on.
    pub(crate) fn read_post(&mut self, max: usize) -> Vec<u8> {
        let body = self
            .description
            .web
            .as_ref()
            .map_or(&[][..], |web| web.body.as_slice());
        let from = self.post.read.min(body.len());
        let to = from.saturating_add(max).min(body.len());
        let block = body[from..to].to_vec();
        self.post.read = to;
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
        match self.post.entry {
            Some(PostEntry::Form) => {
                let body = self.post.request_body.clone().unwrap_or_default();
                self.add_post_vars(array, &body, limits)
            }
            Some(PostEntry::Multipart) => {
                crate::rfc1867::post_handler(self, &content_type, array, limits)
            }
            None => Ok(()),
        }
    }

    /// What `php://input` reads (`Z/ext/standard/php_fopen_wrapper.c:77`):
    /// the body the form reader buffered, with what the host has not
    /// handed out read after it, which becomes the request's body; empty
    /// where a reader took the body and kept none of it, as multipart
    /// does.
    pub(crate) fn input_bytes(&mut self) -> Vec<u8> {
        let mut body = self.post.request_body.take().unwrap_or_default();
        if !self.post.done {
            let rest = self.read_post_block(usize::MAX);
            body.extend_from_slice(&rest);
        }
        self.post.request_body = Some(body.clone());
        body
    }

    /// `request_parse_body()`'s body (`Z/ext/standard/http.c:345`): the
    /// content type's reader over the body, under `limits` where the
    /// caller named them, and its handler into two fresh arrays, the
    /// `$_POST` and `$_FILES` it would have made, which the request's own
    /// are not; a request with no content type, or one no handler reads,
    /// is php's `RequestParseBodyException`.
    pub fn parse_body(&mut self, limits: &BodyLimits) -> Result<(PhpArray, PhpArray), Thrown> {
        let Some(content_type) = self.web().and_then(|web| web.content_type.clone()) else {
            return Err(throw(
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
            return Err(throw(self, "RequestParseBodyException", &text));
        }
        let post = PhpArray::new(self)?;
        let files = PhpArray::new(self)?;
        let kept = self.http_globals.files.replace(files);
        let throwing = std::mem::replace(&mut self.post.throwing, true);
        let handled = self.handle_post(post, limits);
        self.post.throwing = throwing;
        let files = std::mem::replace(&mut self.http_globals.files, kept).unwrap_or(files);
        match handled {
            Ok(()) if !self.is_pending() => Ok((post, files)),
            Ok(()) => {
                self.release(crate::api::values::Value::array(post));
                self.release(crate::api::values::Value::array(files));
                Err(Thrown::of(None))
            }
            Err(thrown) => {
                self.release(crate::api::values::Value::array(post));
                self.release(crate::api::values::Value::array(files));
                Err(thrown)
            }
        }
    }

    /// Whether `path` is a file an upload of this request wrote and no
    /// `move_uploaded_file()` has moved, `SG(rfc1867_uploaded_files)`.
    #[must_use]
    pub fn is_uploaded_file(&self, path: &[u8]) -> bool {
        self.post
            .uploaded
            .iter()
            .any(|file| file.as_os_str().as_encoded_bytes() == path)
    }

    /// `move_uploaded_file()`'s first half: take the upload's temporary
    /// path out of the request, once.
    pub fn take_uploaded_file(&mut self, path: &[u8]) -> Option<super::HostPath> {
        let at = self
            .post
            .uploaded
            .iter()
            .position(|file| file.as_os_str().as_encoded_bytes() == path)?;
        Some(super::HostPath(self.post.uploaded.remove(at)))
    }

    /// `destroy_uploaded_files_hash` (`Z/main/rfc1867.c:170`), at
    /// `sapi_deactivate_destroy`: every uploaded file no
    /// `move_uploaded_file()` moved, removed.
    pub(crate) fn destroy_uploaded_files(&mut self) {
        for file in std::mem::take(&mut self.post.uploaded) {
            let _ = std::fs::remove_file(file);
        }
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

    /// One file an upload wrote, kept for `is_uploaded_file()`.
    pub(crate) fn note_uploaded(&mut self, path: PathBuf) {
        self.post.uploaded.push(path);
    }
}
