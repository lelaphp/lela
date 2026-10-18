//! `FastCGI`'s connection as php's `main/fastcgi.c` keeps it for the two
//! hosts that speak it, php-fpm's children and php-cgi under `-b`: one
//! accepted connection over TCP or a Unix socket, the web servers allowed
//! to connect (`fcgi_set_allowed_clients`, `fcgi_is_allowed`) and the
//! peer's address as a log names it (`fcgi_get_last_client_ip`). The
//! records are `php_proto::fcgi`'s.

use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;

/// One connection a web server opened.
pub enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(buf),
            Conn::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.write(buf),
            Conn::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Conn {
    /// The connection's `fd`, which a poll waits on.
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        match self {
            Conn::Tcp(s) => s.as_fd(),
            Conn::Unix(s) => s.as_fd(),
        }
    }

    /// `fcgi_close(req, force, …)`: the connection shut and drained where
    /// not forced.
    pub fn close(self, force: bool) {
        if !force {
            match &self {
                Conn::Tcp(s) => s.shutdown(Shutdown::Write),
                Conn::Unix(s) => s.shutdown(Shutdown::Write),
            }
            .ok();
            let mut this = self;
            let mut buf = [0u8; 8];
            while matches!(this.read(&mut buf), Ok(n) if n > 0) {}
        }
    }
}

/// `fcgi_set_allowed_clients`: the addresses `list` names, a comma
/// between two, each one that is no address handed to `wrong` for the
/// host to log as its own; an empty list lets nothing in.
pub fn allowed_clients(list: &[u8], mut wrong: impl FnMut(&[u8])) -> Vec<IpAddr> {
    let mut allowed = Vec::new();
    for word in list.split(|b| *b == b',') {
        match std::str::from_utf8(word)
            .ok()
            .and_then(|w| w.parse::<IpAddr>().ok())
        {
            Some(ip) => allowed.push(ip),
            None => wrong(word),
        }
    }
    allowed
}

/// `fcgi_is_allowed`: whether `peer` may connect, every peer where no
/// list was set and every connection over a Unix socket, which names no
/// peer.
#[must_use]
pub fn allowed(allowed: Option<&[IpAddr]>, peer: Option<IpAddr>) -> bool {
    let (Some(list), Some(peer)) = (allowed, peer) else {
        return true;
    };
    list.iter().any(|ip| match (ip, peer) {
        (IpAddr::V4(a), IpAddr::V4(p)) => *a == p,
        (IpAddr::V6(a), IpAddr::V6(p)) => a.octets()[..12] == p.octets()[..12],
        (IpAddr::V4(a), IpAddr::V6(p)) => p.to_ipv4_mapped() == Some(*a),
        (IpAddr::V6(_), IpAddr::V4(_)) => false,
    })
}

/// `fcgi_get_last_client_ip`: the peer, a mapped IPv4 as IPv4.
#[must_use]
pub fn client_ip(peer: Option<IpAddr>) -> Option<String> {
    peer.map(|ip| match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
        IpAddr::V4(v4) => v4.to_string(),
    })
}
