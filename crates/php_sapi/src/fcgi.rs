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

/// `fcgi_set_allowed_clients`, and `fcgi_listen`'s read of
/// `FCGI_WEB_SERVER_ADDRS`, which is the same: the addresses `list`
/// names, a comma between two. Each word that is no address is handed to
/// `log`, the host's `fcgi_log`, as `Wrong IP address '<word>' in
/// listen.allowed_clients`, and a list that names none as `There are no
/// allowed addresses`, with no newline after either
/// (`Z/main/fastcgi.c:793`, `:800`, `:853`, `:860`); an empty list lets
/// nothing in.
pub fn allowed_clients(list: &[u8], mut log: impl FnMut(&[u8])) -> Vec<IpAddr> {
    let mut allowed = Vec::new();
    for word in list.split(|b| *b == b',') {
        match std::str::from_utf8(word)
            .ok()
            .and_then(|w| w.parse::<IpAddr>().ok())
        {
            Some(ip) => allowed.push(ip),
            None => log(&[
                &b"Wrong IP address '"[..],
                word,
                b"' in listen.allowed_clients",
            ]
            .concat()),
        }
    }
    if allowed.is_empty() {
        log(b"There are no allowed addresses");
    }
    allowed
}

/// `fcgi_accept_request`'s check of an accepted connection: whether
/// `peer` may connect, as `fcgi_is_allowed` judges it (every peer where
/// no list was set, every connection over a Unix socket, which names no
/// peer, and otherwise a peer the list names), one that may not handed to `log`,
/// the host's `fcgi_log`, as `Connection disallowed: IP address '<peer>'
/// has been dropped.` with no newline after it
/// (`Z/main/fastcgi.c:1402`), for the host to close.
#[must_use]
pub fn admitted(list: Option<&[IpAddr]>, peer: Option<IpAddr>, log: impl FnOnce(&[u8])) -> bool {
    if allowed(list, peer) {
        return true;
    }
    log(format!(
        "Connection disallowed: IP address '{}' has been dropped.",
        client_ip(peer).unwrap_or_default()
    )
    .as_bytes());
    false
}

/// `fcgi_is_allowed`: whether `peer` may connect, every peer where no
/// list was set and every connection over a Unix socket, which names no
/// peer. An address is let in by an equal one of its family, all four
/// octets of an IPv4 and all sixteen of an IPv6 (`sizeof(sin6_addr)`,
/// `Z/main/fastcgi.c:1334`), and an IPv4-mapped IPv6 peer by the IPv4
/// it maps, its last four octets (`Z/main/fastcgi.c:1338-1341`).
fn allowed(allowed: Option<&[IpAddr]>, peer: Option<IpAddr>) -> bool {
    let (Some(list), Some(peer)) = (allowed, peer) else {
        return true;
    };
    list.iter().any(|ip| match (ip, peer) {
        (IpAddr::V4(a), IpAddr::V4(p)) => *a == p,
        (IpAddr::V6(a), IpAddr::V6(p)) => *a == p,
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

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::{admitted, allowed, allowed_clients};

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("the test names an address")
    }

    fn lets_in(list: &[&str], peer: &str) -> bool {
        let list: Vec<IpAddr> = list.iter().map(|text| ip(text)).collect();
        allowed(Some(&list), Some(ip(peer)))
    }

    /// An IPv6 peer is let in by the address it is, all sixteen octets of
    /// it, as `fcgi_is_allowed`'s `memcmp` of `sizeof(sin6_addr)` judges
    /// it: one in the allowed address's /96 that differs in its last 32
    /// bits is refused.
    #[test]
    fn an_ipv6_peer_is_let_in_by_all_sixteen_octets() {
        assert!(lets_in(&["2001:db8::1"], "2001:db8::1"));
        assert!(!lets_in(&["2001:db8::1"], "2001:db8::2"));
        assert!(!lets_in(&["2001:db8::1"], "2001:db8::ffff:ffff"));
        assert!(!lets_in(&["::2"], "::1"));
        assert!(lets_in(&["::2", "::1"], "::1"));
    }

    /// An IPv4-mapped IPv6 peer is let in by the IPv4 it maps or by its
    /// own IPv6, and an IPv4 peer never by an IPv6, mapped or not.
    #[test]
    fn a_mapped_peer_is_let_in_by_its_ipv4_and_an_ipv4_peer_by_no_ipv6() {
        assert!(lets_in(&["192.0.2.1"], "::ffff:192.0.2.1"));
        assert!(!lets_in(&["192.0.2.1"], "::ffff:192.0.2.2"));
        assert!(lets_in(&["::ffff:192.0.2.1"], "::ffff:192.0.2.1"));
        assert!(!lets_in(&["::ffff:192.0.2.1"], "192.0.2.1"));
        assert!(!lets_in(&["192.0.2.1"], "::192.0.2.1"));
        assert!(lets_in(&["192.0.2.1"], "192.0.2.1"));
        assert!(!lets_in(&["192.0.2.1"], "192.0.2.2"));
    }

    /// No list lets every peer in, a Unix socket's connection names no
    /// peer and is let in, and an empty list lets no peer in.
    #[test]
    fn no_list_and_no_peer_let_in_and_an_empty_list_does_not() {
        assert!(allowed(None, Some(ip("::1"))));
        assert!(allowed(Some(&[]), None));
        assert!(!allowed(Some(&[]), Some(ip("::1"))));
    }

    /// What `list` logs, each message as the host's `fcgi_log` is handed
    /// it, and the addresses it names.
    fn logged(list: &[u8]) -> (Vec<String>, Vec<IpAddr>) {
        let mut log = Vec::new();
        let named = allowed_clients(list, |message| {
            log.push(String::from_utf8_lossy(message).into_owned());
        });
        (log, named)
    }

    /// Each word that is no address is logged with php's words, and a
    /// list naming none, the empty one among them, is logged once more.
    #[test]
    fn a_wrong_word_and_a_list_naming_none_are_logged_with_php_s_words() {
        let wrong = |word: &str| format!("Wrong IP address '{word}' in listen.allowed_clients");
        let none = "There are no allowed addresses".to_owned();
        assert_eq!(
            logged(b"bogus,::1x"),
            (vec![wrong("bogus"), wrong("::1x"), none.clone()], vec![])
        );
        assert_eq!(logged(b""), (vec![wrong(""), none], vec![]));
        assert_eq!(
            logged(b"127.0.0.1,,::1"),
            (vec![wrong("")], vec![ip("127.0.0.1"), ip("::1")])
        );
    }

    /// A peer the list refuses is logged by the address a log names, a
    /// mapped IPv4 as its IPv4, and one it lets in is not.
    #[test]
    fn a_refused_peer_is_logged_by_its_address() {
        let mut log = Vec::new();
        let list = [ip("192.0.2.1")];
        assert!(!admitted(Some(&list), Some(ip("::ffff:127.0.0.1")), |m| {
            log.push(m.to_vec());
        }));
        assert!(admitted(Some(&list), Some(ip("192.0.2.1")), |m| {
            log.push(m.to_vec());
        }));
        assert_eq!(
            log,
            [b"Connection disallowed: IP address '127.0.0.1' has been dropped.".to_vec()]
        );
    }
}
