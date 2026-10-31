//! `fpm_sockets.c`: each pool's listening socket, made once per address
//! and shared by the pools that name it, TCP by `host:port` or a `port`
//! alone, the IPv6 any-address first where no host is named, or a UNIX
//! socket by path under the pool's owner, group and `listen.mode`; the sockets a
//! reload inherits by the numbers `FPM_SOCKETS` names, and the variable
//! written for the next exec.

use std::ffi::OsStr;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;

use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, SockaddrStorage, UnixAddr, bind, listen,
    setsockopt, socket, sockopt,
};

use crate::conf::atoi;
use crate::{zlog, zlog_sys};

/// `enum fpm_address_domain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Domain {
    Unix,
    Inet,
}

/// `fpm_sockets_domain_from_address`: TCP where the address holds a `:`
/// or is digits alone, a UNIX path otherwise.
pub(crate) fn domain_of(address: &[u8]) -> Domain {
    if address.contains(&b':') || address.iter().all(u8::is_ascii_digit) {
        Domain::Inet
    } else {
        Domain::Unix
    }
}

/// One `struct listening_socket_s` of `sockets_list`.
pub(crate) struct Listening {
    pub(crate) refcount: u32,
    pub(crate) sock: OwnedFd,
    pub(crate) domain: Domain,
    pub(crate) key: Vec<u8>,
}

/// How a pool's UNIX socket is made, `wp->socket_uid`, `_gid`, `_mode`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Permissions {
    pub(crate) uid: Option<u32>,
    pub(crate) gid: Option<u32>,
    pub(crate) mode: u32,
}

/// `sockets_list`.
#[derive(Default)]
pub(crate) struct Sockets {
    pub(crate) list: Vec<Listening>,
}

/// An address a pool listens on.
enum Address {
    Inet(std::net::SocketAddr),
    Unix(UnixAddr),
}

impl Address {
    /// The key `fpm_sockets_hash_op` names an address by: `ip:port`, or
    /// the path.
    fn key(&self) -> Vec<u8> {
        match self {
            Address::Inet(inet) => format!("{}:{}", inet.ip(), inet.port()).into_bytes(),
            Address::Unix(unix) => unix
                .path()
                .map(|p| p.as_os_str().as_bytes().to_vec())
                .unwrap_or_default(),
        }
    }
}

impl Sockets {
    /// `fpm_sockets_init_main`'s import: each `key=fd` of `FPM_SOCKETS`,
    /// `FPM_SOCKETS_1` and on, stored unused.
    pub(crate) fn inherit(&mut self) {
        for i in 0..256 {
            let name = if i == 0 {
                String::from("FPM_SOCKETS")
            } else {
                format!("FPM_SOCKETS_{i}")
            };
            let Some(inherited) = std::env::var_os(&name) else {
                break;
            };
            for pair in inherited.as_bytes().split(|b| *b == b',') {
                let Some(eq) = pair.iter().position(|b| *b == b'=') else {
                    continue;
                };
                let key = pair[..eq.min(255)].to_vec();
                let fd = atoi(&pair[eq + 1..]);
                zlog!(
                    Notice @ fpm_sockets_init_main:485,
                    "using inherited socket fd={fd}, \"{}\"",
                    String::from_utf8_lossy(&key)
                );
                if let Some(sock) = crate::ffi::inherited(fd) {
                    self.list.push(Listening {
                        refcount: 0,
                        sock,
                        domain: domain_of(&key),
                        key,
                    });
                }
            }
        }
    }

    /// `fpm_sockets_hash_op(FPM_GET_USE_SOCKET)`: the socket a key already
    /// stands for, one more pool using it.
    fn use_socket(&mut self, key: &[u8]) -> Option<RawFd> {
        let listening = self.list.iter_mut().find(|l| l.key == key)?;
        listening.refcount += 1;
        Some(listening.sock.as_raw_fd())
    }

    /// `fpm_sockets_get_listening_socket`: the socket for `address`, made
    /// where none stands.
    fn get(
        &mut self,
        address: &Address,
        name: &[u8],
        listen_address: &[u8],
        backlog: i32,
        domain: Domain,
        permissions: Permissions,
    ) -> Option<RawFd> {
        let key = address.key();
        if let Some(fd) = self.use_socket(&key) {
            return Some(fd);
        }
        let sock = new_listening(address, name, listen_address, backlog, domain, permissions)?;
        let fd = sock.as_raw_fd();
        self.list.push(Listening {
            refcount: 1,
            sock,
            domain,
            key,
        });
        Some(fd)
    }

    /// `fpm_socket_af_inet_listening_socket`: `host:port`, or a `port`
    /// alone bound to `::` and, where that fails, `0.0.0.0`.
    pub(crate) fn inet(&mut self, name: &[u8], address: &[u8], backlog: i32) -> Option<RawFd> {
        let (host, port_str) = match address.iter().rposition(|b| *b == b':') {
            Some(colon) => {
                let mut host = &address[..colon];
                if host.first() == Some(&b'[') && host.last() == Some(&b']') {
                    host = &host[1..host.len() - 1];
                }
                (Some(host), &address[colon + 1..])
            }
            None if address.iter().all(u8::is_ascii_digit) => (None, address),
            None => (None, &b""[..]),
        };
        let port = atoi(port_str);
        if !(1..=65535).contains(&port) {
            zlog!(
                Error @ fpm_socket_af_inet_listening_socket:365,
                "invalid port value '{}'",
                String::from_utf8_lossy(port_str)
            );
            return None;
        }
        let port = u16::try_from(port).unwrap_or(0);
        let permissions = Permissions {
            uid: None,
            gid: None,
            mode: 0o660,
        };
        if let Some(host) = host {
            return self.by_addr(name, address, host, port, backlog, permissions);
        }
        let old = crate::zlog::set_level(crate::zlog::Level::Alert);
        let sock = self.by_addr(name, address, b"::", port, backlog, permissions);
        crate::zlog::set_level(old);
        if sock.is_some() {
            return sock;
        }
        zlog!(
            Notice @ fpm_socket_af_inet_listening_socket:386,
            "Failed implicitly binding to ::, retrying with 0.0.0.0"
        );
        self.by_addr(name, address, b"0.0.0.0", port, backlog, permissions)
    }

    /// `fpm_socket_af_inet_socket_by_addr`: the host resolved as
    /// `getaddrinfo` resolves it, the first address bound and every other
    /// named in a warning.
    fn by_addr(
        &mut self,
        name: &[u8],
        listen_address: &[u8],
        host: &[u8],
        port: u16,
        backlog: i32,
        permissions: Permissions,
    ) -> Option<RawFd> {
        use std::net::ToSocketAddrs;
        let host_text = String::from_utf8_lossy(host).into_owned();
        let found: Vec<std::net::SocketAddr> = match (host_text.as_str(), port).to_socket_addrs() {
            Ok(found) => found.collect(),
            Err(e) => {
                zlog!(Error @ fpm_socket_af_inet_socket_by_addr:317, "getaddrinfo: {}", gai_text(&e));
                return None;
            }
        };
        let mut sock = None;
        for address in found {
            if sock.is_none() {
                sock = self.get(
                    &Address::Inet(address),
                    name,
                    listen_address,
                    backlog,
                    Domain::Inet,
                    permissions,
                );
                if sock.is_some() {
                    zlog!(
                        Debug @ fpm_socket_af_inet_socket_by_addr:325,
                        "Found address for {host_text}, socket opened on {}",
                        address.ip()
                    );
                }
            } else {
                zlog!(
                    Warning @ fpm_socket_af_inet_socket_by_addr:328,
                    "Found multiple addresses for {host_text}, {} ignored",
                    address.ip()
                );
            }
        }
        sock
    }

    /// `fpm_socket_af_unix_listening_socket`: the path, cut to what
    /// `sun_path` holds with a warning where it is longer.
    pub(crate) fn unix(
        &mut self,
        name: &[u8],
        address: &[u8],
        backlog: i32,
        permissions: Permissions,
    ) -> Option<RawFd> {
        const SUN_PATH: usize = if cfg!(target_os = "linux") { 108 } else { 104 };
        let cut = &address[..address.len().min(SUN_PATH - 1)];
        if address.len() >= SUN_PATH {
            zlog!(
                Warning @ fpm_socket_af_unix_listening_socket:416,
                "[pool {}] cannot bind to UNIX socket '{}' as path is too long (found length: {}, maximal length: {SUN_PATH}), trying cut socket path instead '{}'",
                String::from_utf8_lossy(name),
                String::from_utf8_lossy(address),
                address.len(),
                String::from_utf8_lossy(cut)
            );
        }
        let addr = UnixAddr::new(OsStr::from_bytes(cut)).ok()?;
        self.get(
            &Address::Unix(addr),
            name,
            address,
            backlog,
            Domain::Unix,
            permissions,
        )
    }

    /// `fpm_sockets_init_main`'s end: every inherited socket no pool took
    /// closed, a UNIX socket's path unlinked.
    pub(crate) fn close_unused(&mut self) {
        self.list.retain(|listening| {
            if listening.refcount > 0 {
                return true;
            }
            if listening.domain == Domain::Unix {
                std::fs::remove_file(OsStr::from_bytes(&listening.key)).ok();
            }
            false
        });
    }

    /// `fpm_sockets_cleanup(FPM_CLEANUP_PARENT_EXEC)`: `FPM_SOCKETS` and
    /// its sets for the process an exec makes, each `key=fd` pair, in
    /// sets of `FPM_ENV_SOCKET_SET_SIZE`, the descriptors left open
    /// across the exec.
    pub(crate) fn for_exec(&self) -> Vec<(String, Vec<u8>)> {
        const SET_SIZE: usize = 128;
        let mut sets: Vec<Vec<u8>> = Vec::new();
        for (i, listening) in self.list.iter().enumerate() {
            if i % SET_SIZE == 0 {
                sets.push(Vec::new());
            }
            let Some(set) = sets.last_mut() else {
                continue;
            };
            if !set.is_empty() {
                set.push(b',');
            }
            set.extend_from_slice(&listening.key);
            set.push(b'=');
            set.extend_from_slice(listening.sock.as_raw_fd().to_string().as_bytes());
            nix::fcntl::fcntl(
                listening.sock.as_fd(),
                nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
            )
            .ok();
        }
        let mut vars: Vec<(String, Vec<u8>)> = sets
            .into_iter()
            .enumerate()
            .map(|(i, set)| {
                (
                    if i == 0 {
                        String::from("FPM_SOCKETS")
                    } else {
                        format!("FPM_SOCKETS_{i}")
                    },
                    set,
                )
            })
            .collect();
        let end = vars.len();
        vars.push((
            if end == 0 {
                String::from("FPM_SOCKETS")
            } else {
                format!("FPM_SOCKETS_{end}")
            },
            Vec::new(),
        ));
        vars
    }

    /// `fpm_sockets_cleanup(FPM_CLEANUP_PARENT_EXIT_MAIN)`: each UNIX
    /// socket's path unlinked as the master exits.
    pub(crate) fn unlink_all(&self) {
        for listening in &self.list {
            if listening.domain == Domain::Unix {
                std::fs::remove_file(OsStr::from_bytes(&listening.key)).ok();
            }
        }
    }

    /// The socket at `fd`, what a pool's `listening_socket` names.
    pub(crate) fn by_fd(&self, fd: RawFd) -> Option<&OwnedFd> {
        self.list
            .iter()
            .find(|l| l.sock.as_raw_fd() == fd)
            .map(|l| &l.sock)
    }
}

/// `gai_strerror`'s text of a resolution failure.
fn gai_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    text.strip_prefix("failed to lookup address information: ")
        .map_or(text.clone(), str::to_owned)
}

/// `fpm_sockets_new_listening_socket`: the socket made, `SO_REUSEADDR`
/// set, a UNIX path refused where another php-fpm answers on it and
/// otherwise unlinked and bound under the pool's `listen.mode`, owner and group,
/// and listened on with the pool's `listen.backlog`.
fn new_listening(
    address: &Address,
    name: &[u8],
    listen_address: &[u8],
    backlog: i32,
    domain: Domain,
    permissions: Permissions,
) -> Option<OwnedFd> {
    let family = match address {
        Address::Inet(std::net::SocketAddr::V4(_)) => AddressFamily::Inet,
        Address::Inet(std::net::SocketAddr::V6(_)) => AddressFamily::Inet6,
        Address::Unix(_) => AddressFamily::Unix,
    };
    let sock = match socket(family, SockType::Stream, SockFlag::empty(), None) {
        Ok(sock) => sock,
        Err(e) => {
            zlog_sys!(Error @ fpm_sockets_new_listening_socket:214, e, "failed to create new listening socket: socket()");
            return None;
        }
    };
    if setsockopt(&sock, sockopt::ReuseAddr, &true).is_err() {
        zlog!(Warning @ fpm_sockets_new_listening_socket:219, "failed to change socket attribute");
    }
    let path = address.key();
    let mut saved_umask = None;
    if domain == Domain::Unix {
        if std::os::unix::net::UnixStream::connect(OsStr::from_bytes(&path)).is_ok() {
            zlog!(
                Error @ fpm_sockets_new_listening_socket:224,
                "Another FPM instance seems to already listen on {}",
                String::from_utf8_lossy(&path)
            );
            return None;
        }
        std::fs::remove_file(OsStr::from_bytes(&path)).ok();
        let mask = nix::sys::stat::Mode::from_bits_truncate(
            nix::libc::mode_t::try_from(0o777 ^ permissions.mode).unwrap_or(0),
        );
        saved_umask = Some(nix::sys::stat::umask(mask));
    }
    let bound = match address {
        Address::Inet(inet) => bind(sock.as_raw_fd(), &SockaddrStorage::from(*inet)),
        Address::Unix(unix) => bind(sock.as_raw_fd(), unix),
    };
    if let Err(e) = bound {
        zlog_sys!(
            Error @ fpm_sockets_new_listening_socket:233,
            e,
            "unable to bind listening socket for address '{}'",
            String::from_utf8_lossy(listen_address)
        );
        if let Some(mask) = saved_umask {
            nix::sys::stat::umask(mask);
        }
        return None;
    }
    if let Some(mask) = saved_umask {
        nix::sys::stat::umask(mask);
        if !crate::unix::set_socket_permissions(name, &path, listen_address, permissions) {
            return None;
        }
    }
    let backlog = Backlog::new(backlog).unwrap_or(Backlog::MAXCONN);
    if let Err(e) = listen(&sock, backlog) {
        zlog_sys!(
            Error @ fpm_sockets_new_listening_socket:253,
            e,
            "failed to listen to address '{}'",
            String::from_utf8_lossy(listen_address)
        );
        return None;
    }
    Some(sock)
}
