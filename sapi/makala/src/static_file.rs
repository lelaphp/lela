//! A static file, sent from the connection side (#2587), as nginx sends
//! one in front of php-fpm with no PHP worker held: no request thread
//! waits on a client that takes a large file slowly, so a handful of slow
//! downloads cannot starve the bounded pool of its programs. The disk
//! blocks, so the connection side's one thread never reads it (M4): the
//! file is opened on tokio's blocking pool as the request is routed
//! ([`crate::route::Router::route`]), and each piece is read there as
//! the client takes the last (`tokio::fs::File`), so a slow client holds
//! an open fd and one piece, and no thread. The response's
//! minimum data rate bounds it as it bounds a program's.

use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::body::Bytes;
use php_proto::vpath::{self, Translated};
use php_sapi::{HostPath, ResponseHead};
use tokio::io::{AsyncRead, ReadBuf};

use crate::connection::Unfinished;

/// How many bytes of a static file are read for one write to the
/// connection.
const PIECE: usize = 64 << 10;

/// A static file opened for its response, as `php -S` serves one
/// (`php_cli_server_begin_send_static`, `Z/sapi/cli/php_cli_server.c:2144`):
/// the type its extension has in php's table and the length of the file
/// opened, and that many of its bytes after them, so a file that grows
/// while it goes is sent as it was and one that shrinks cuts its
/// connection.
pub(crate) struct StaticFile {
    file: std::fs::File,
    length: u64,
    content_type: Option<String>,
}

impl StaticFile {
    /// The file `found` names, at the `host` path the `open_basedir` guard
    /// let it open, opened, and its length; none where it does not open,
    /// which is answered as nothing there. It waits on the disk.
    pub(crate) fn open(host: &HostPath, found: &Translated) -> Option<StaticFile> {
        let file = std::fs::File::open(host).ok()?;
        let length = file.metadata().ok()?.len();
        Some(StaticFile {
            file,
            length,
            content_type: php_proto::mime::content_type(vpath::extension(&found.vpath)),
        })
    }

    /// The response's head: 200, the file's type where its extension has
    /// one, and its length.
    pub(crate) fn head(&self) -> ResponseHead {
        let mut lines = Vec::new();
        if let Some(content_type) = &self.content_type {
            lines.push(format!("Content-Type: {content_type}").into_bytes());
        }
        lines.push(format!("Content-Length: {}", self.length).into_bytes());
        ResponseHead {
            code: 200,
            lines,
            ..ResponseHead::default()
        }
    }

    /// The response's body: the file's bytes as far as its length.
    pub(crate) fn body(self) -> Sending {
        Sending {
            file: tokio::fs::File::from_std(self.file),
            left: self.length,
            piece: Vec::new(),
        }
    }
}

/// A static file's body as the connection takes it, one piece read for
/// each the client has taken.
pub(crate) struct Sending {
    file: tokio::fs::File,
    /// The bytes of its length not yet read.
    left: u64,
    /// The piece being read, kept across a read that waits on the disk.
    piece: Vec<u8>,
}

impl Sending {
    /// The next piece, none once the length has gone, or the file's
    /// failure: it ended short of its length, or its read failed.
    pub(crate) fn poll_piece(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Unfinished>>> {
        if self.left == 0 {
            return Poll::Ready(None);
        }
        let want = usize::try_from(self.left).map_or(PIECE, |left| left.min(PIECE));
        self.piece.resize(want, 0);
        let mut buf = ReadBuf::new(&mut self.piece);
        if let Err(failed) = std::task::ready!(Pin::new(&mut self.file).poll_read(cx, &mut buf)) {
            return Poll::Ready(Some(Err(Unfinished::ReadFailed(failed))));
        }
        let read = buf.filled().len();
        if read == 0 {
            return Poll::Ready(Some(Err(Unfinished::Shrank)));
        }
        self.left = self
            .left
            .saturating_sub(u64::try_from(read).unwrap_or(u64::MAX));
        let mut piece = std::mem::take(&mut self.piece);
        piece.truncate(read);
        Poll::Ready(Some(Ok(Bytes::from(piece))))
    }

    /// The bytes still to send.
    pub(crate) fn left(&self) -> u64 {
        self.left
    }
}
