//! A connection's socket as hyper reads and writes it, and how it ends.
//!
//! Closing a socket that still holds bytes the client sent and the server
//! never read makes the kernel reset the connection, and a reset can
//! destroy the last response before the client reads it: the 400 that
//! refuses a request whose body the server will not read, or the answer
//! before a close the framing asks for. So a socket is not closed where
//! hyper lets go of it. It lingers, as Cowboy's `linger_timeout` has a
//! connection linger: its write side is shut, which the client reads as
//! the response's end, and what the client still sends is read and
//! dropped until it closes its side or [`LINGER`] has passed. A socket
//! let go outside the runtime, which only the process's end does, closes
//! at once. A lingering socket holds its descriptor for that second
//! beyond its connection.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// How long a closing socket reads and drops what its client still
/// sends: Cowboy's `linger_timeout`, 1000 ms.
const LINGER: Duration = Duration::from_secs(1);

/// The socket of one connection, which lingers when it is let go.
pub(crate) struct Socket {
    /// Taken only when the socket is let go.
    stream: Option<TcpStream>,
}

impl Socket {
    /// The socket of a connection just accepted.
    pub(crate) fn new(stream: TcpStream) -> Socket {
        Socket {
            stream: Some(stream),
        }
    }

    /// The stream, which is gone only once the socket is let go.
    fn stream(&mut self) -> io::Result<Pin<&mut TcpStream>> {
        self.stream
            .as_mut()
            .map(Pin::new)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))
    }
}

impl AsyncRead for Socket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut().stream()?.poll_read(cx, buf)
    }
}

impl AsyncWrite for Socket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().stream()?.poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().stream()?.poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream
            .as_ref()
            .is_some_and(TcpStream::is_write_vectored)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().stream()?.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().stream()?.poll_shutdown(cx)
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if let (Some(stream), Ok(runtime)) =
            (self.stream.take(), tokio::runtime::Handle::try_current())
        {
            runtime.spawn(linger(stream));
        }
    }
}

/// The write side shut, then what the client still sends read and
/// dropped until it closes or [`LINGER`] passes, and the socket closed.
async fn linger(mut stream: TcpStream) {
    std::future::poll_fn(|cx| Pin::new(&mut stream).poll_shutdown(cx))
        .await
        .ok();
    let drained = async {
        let mut dropped = [0u8; 4096];
        loop {
            if stream.readable().await.is_err() {
                return;
            }
            match stream.try_read(&mut dropped) {
                Ok(0) => return,
                Err(e) if e.kind() != io::ErrorKind::WouldBlock => return,
                _ => {}
            }
        }
    };
    tokio::time::timeout(LINGER, drained).await.ok();
}
