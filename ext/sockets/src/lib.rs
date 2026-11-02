//! The `sockets` extension: `php-src/ext/sockets` as one crate under
//! `ext/` (ADR-0004). One workspace dependency, the facade; the OS
//! through `nix`, and through `libc` in [`ffi`] alone for what `nix` does
//! not wrap in the form php makes the call (design §7.2, revision 36);
//! one entry, `extension()`; its state on the value (ADR-0005). The phpt
//! subset is `php-src/ext/sockets/tests`, through `xtask phpt --subset`.
//!
//! Every native is `sockets.c`'s, `sendrecvmsg.c`'s or `multicast.c`'s at
//! the tag, declared in `sockets.stub.php`'s order, and what php declares
//! under a platform's `#ifdef` is declared under the platform's `#[cfg]`.
//! A socket imported from a stream or exported to one shares the
//! engine's stream: the stream owns the fd and the socket one
//! owner of the stream, as `zstream` is (`socket_import_stream`).
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod addr;
mod addrinfo;
mod conv;
mod ffi;
#[cfg(all(test, target_os = "linux"))]
mod linux_values;
mod msg;
mod natives;
mod options;
mod socket;
mod stream;
mod throwables;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_const;
use php_ext::request::Request;

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. php's `zend_sockets_globals`, less the `strerror`
/// buffer, which a Rust string replaces.
#[derive(Default)]
struct State {
    /// `SOCKETS_G(last_error)`.
    last_error: i32,
}

/// The constants `register_sockets_symbols` registers, in the stub's
/// order, each under the stub's `#ifdef`: what the platform's headers
/// name.
///
/// ```php
/// var_dump(AF_INET, SOCK_STREAM, SOL_TCP, TCP_NODELAY, SOCKET_EAGAIN === SOCKET_EWOULDBLOCK, MCAST_JOIN_GROUP, PHP_NORMAL_READ, SHUT_RDWR);
/// ```
/// ```expect
/// int(2)
/// int(1)
/// int(6)
/// int(1)
/// bool(true)
/// int(12)
/// int(1)
/// int(2)
/// ```
#[php_const]
mod constants {
    pub(crate) const AF_UNIX: i64 = libc::AF_UNIX as i64;
    pub(crate) const AF_INET: i64 = libc::AF_INET as i64;
    pub(crate) const AF_INET6: i64 = libc::AF_INET6 as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const AF_PACKET: i64 = libc::AF_PACKET as i64;
    pub(crate) const SOCK_STREAM: i64 = libc::SOCK_STREAM as i64;
    pub(crate) const SOCK_DGRAM: i64 = libc::SOCK_DGRAM as i64;
    pub(crate) const SOCK_RAW: i64 = libc::SOCK_RAW as i64;
    pub(crate) const SOCK_SEQPACKET: i64 = libc::SOCK_SEQPACKET as i64;
    pub(crate) const SOCK_RDM: i64 = libc::SOCK_RDM as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCK_DCCP: i64 = libc::SOCK_DCCP as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCK_CLOEXEC: i64 = libc::SOCK_CLOEXEC as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCK_NONBLOCK: i64 = libc::SOCK_NONBLOCK as i64;
    pub(crate) const MSG_OOB: i64 = libc::MSG_OOB as i64;
    pub(crate) const MSG_WAITALL: i64 = libc::MSG_WAITALL as i64;
    pub(crate) const MSG_CTRUNC: i64 = libc::MSG_CTRUNC as i64;
    pub(crate) const MSG_TRUNC: i64 = libc::MSG_TRUNC as i64;
    pub(crate) const MSG_PEEK: i64 = libc::MSG_PEEK as i64;
    pub(crate) const MSG_DONTROUTE: i64 = libc::MSG_DONTROUTE as i64;
    pub(crate) const MSG_EOR: i64 = libc::MSG_EOR as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const MSG_EOF: i64 = libc::MSG_EOF as i64;
    /// `php_sockets.h`'s `MSG_EOF`, `MSG_FIN` where the headers have no `MSG_EOF`.
    #[cfg(target_os = "linux")]
    pub(crate) const MSG_EOF: i64 = libc::MSG_FIN as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MSG_CONFIRM: i64 = libc::MSG_CONFIRM as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MSG_ERRQUEUE: i64 = libc::MSG_ERRQUEUE as i64;
    pub(crate) const MSG_NOSIGNAL: i64 = libc::MSG_NOSIGNAL as i64;
    pub(crate) const MSG_DONTWAIT: i64 = libc::MSG_DONTWAIT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MSG_MORE: i64 = libc::MSG_MORE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MSG_WAITFORONE: i64 = libc::MSG_WAITFORONE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MSG_CMSG_CLOEXEC: i64 = libc::MSG_CMSG_CLOEXEC as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MSG_ZEROCOPY: i64 = libc::MSG_ZEROCOPY as i64;
    pub(crate) const SO_DEBUG: i64 = libc::SO_DEBUG as i64;
    pub(crate) const SO_REUSEADDR: i64 = libc::SO_REUSEADDR as i64;
    pub(crate) const SO_REUSEPORT: i64 = libc::SO_REUSEPORT as i64;
    pub(crate) const SO_KEEPALIVE: i64 = libc::SO_KEEPALIVE as i64;
    pub(crate) const SO_DONTROUTE: i64 = libc::SO_DONTROUTE as i64;
    pub(crate) const SO_LINGER: i64 = libc::SO_LINGER as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const SO_LINGER_SEC: i64 = libc::SO_LINGER_SEC as i64;
    pub(crate) const SO_BROADCAST: i64 = libc::SO_BROADCAST as i64;
    pub(crate) const SO_OOBINLINE: i64 = libc::SO_OOBINLINE as i64;
    pub(crate) const SO_SNDBUF: i64 = libc::SO_SNDBUF as i64;
    pub(crate) const SO_RCVBUF: i64 = libc::SO_RCVBUF as i64;
    pub(crate) const SO_SNDLOWAT: i64 = libc::SO_SNDLOWAT as i64;
    pub(crate) const SO_RCVLOWAT: i64 = libc::SO_RCVLOWAT as i64;
    pub(crate) const SO_SNDTIMEO: i64 = libc::SO_SNDTIMEO as i64;
    pub(crate) const SO_RCVTIMEO: i64 = libc::SO_RCVTIMEO as i64;
    pub(crate) const SO_TYPE: i64 = libc::SO_TYPE as i64;
    pub(crate) const SO_ERROR: i64 = libc::SO_ERROR as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const SO_BINDTODEVICE: i64 = 4404;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_BINDTODEVICE: i64 = libc::SO_BINDTODEVICE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_BINDTOIFINDEX: i64 = libc::SO_BINDTOIFINDEX as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const SO_DONTTRUNC: i64 = libc::SO_DONTTRUNC as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const SO_WANTMORE: i64 = libc::SO_WANTMORE as i64;
    pub(crate) const SOL_SOCKET: i64 = libc::SOL_SOCKET as i64;
    pub(crate) const SOMAXCONN: i64 = libc::SOMAXCONN as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_MARK: i64 = libc::SO_MARK as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_INCOMING_CPU: i64 = libc::SO_INCOMING_CPU as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_MEMINFO: i64 = libc::SO_MEMINFO as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_BPF_EXTENSIONS: i64 = libc::SO_BPF_EXTENSIONS as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_BUSY_POLL: i64 = libc::SO_BUSY_POLL as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_OFF: i64 = libc::SKF_AD_OFF as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_PROTOCOL: i64 = libc::SKF_AD_PROTOCOL as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_PKTTYPE: i64 = libc::SKF_AD_PKTTYPE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_IFINDEX: i64 = libc::SKF_AD_IFINDEX as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_NLATTR: i64 = libc::SKF_AD_NLATTR as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_NLATTR_NEST: i64 = libc::SKF_AD_NLATTR_NEST as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_MARK: i64 = libc::SKF_AD_MARK as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_QUEUE: i64 = libc::SKF_AD_QUEUE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_HATYPE: i64 = libc::SKF_AD_HATYPE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_RXHASH: i64 = libc::SKF_AD_RXHASH as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_CPU: i64 = libc::SKF_AD_CPU as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_ALU_XOR_X: i64 = libc::SKF_AD_ALU_XOR_X as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_VLAN_TAG: i64 = libc::SKF_AD_VLAN_TAG as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_VLAN_TAG_PRESENT: i64 = libc::SKF_AD_VLAN_TAG_PRESENT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_PAY_OFFSET: i64 = libc::SKF_AD_PAY_OFFSET as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_RANDOM: i64 = libc::SKF_AD_RANDOM as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_VLAN_TPID: i64 = libc::SKF_AD_VLAN_TPID as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SKF_AD_MAX: i64 = libc::SKF_AD_MAX as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_CONGESTION: i64 = libc::TCP_CONGESTION as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_SYNCNT: i64 = libc::TCP_SYNCNT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_ZEROCOPY: i64 = libc::SO_ZEROCOPY as i64;
    pub(crate) const TCP_NODELAY: i64 = libc::TCP_NODELAY as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const TCP_NOTSENT_LOWAT: i64 = 513;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_NOTSENT_LOWAT: i64 = libc::TCP_NOTSENT_LOWAT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_DEFER_ACCEPT: i64 = libc::TCP_DEFER_ACCEPT as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const TCP_KEEPALIVE: i64 = libc::TCP_KEEPALIVE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_KEEPIDLE: i64 = libc::TCP_KEEPIDLE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_KEEPINTVL: i64 = libc::TCP_KEEPINTVL as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_KEEPCNT: i64 = libc::TCP_KEEPCNT as i64;
    pub(crate) const PHP_NORMAL_READ: i64 = 1;
    pub(crate) const PHP_BINARY_READ: i64 = 2;
    /// `PHP_MCAST_JOIN_GROUP`: `IP_ADD_MEMBERSHIP` where there is no RFC 3678 API.
    #[cfg(target_os = "macos")]
    pub(crate) const MCAST_JOIN_GROUP: i64 = libc::IP_ADD_MEMBERSHIP as i64;
    #[cfg(not(target_os = "macos"))]
    pub(crate) const MCAST_JOIN_GROUP: i64 = libc::MCAST_JOIN_GROUP as i64;
    /// `PHP_MCAST_LEAVE_GROUP`, as the join.
    #[cfg(target_os = "macos")]
    pub(crate) const MCAST_LEAVE_GROUP: i64 = libc::IP_DROP_MEMBERSHIP as i64;
    #[cfg(not(target_os = "macos"))]
    pub(crate) const MCAST_LEAVE_GROUP: i64 = libc::MCAST_LEAVE_GROUP as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MCAST_BLOCK_SOURCE: i64 = libc::MCAST_BLOCK_SOURCE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MCAST_UNBLOCK_SOURCE: i64 = libc::MCAST_UNBLOCK_SOURCE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MCAST_JOIN_SOURCE_GROUP: i64 = libc::MCAST_JOIN_SOURCE_GROUP as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const MCAST_LEAVE_SOURCE_GROUP: i64 = libc::MCAST_LEAVE_SOURCE_GROUP as i64;
    pub(crate) const IP_MULTICAST_IF: i64 = libc::IP_MULTICAST_IF as i64;
    pub(crate) const IP_MULTICAST_TTL: i64 = libc::IP_MULTICAST_TTL as i64;
    pub(crate) const IP_MULTICAST_LOOP: i64 = libc::IP_MULTICAST_LOOP as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const IP_BIND_ADDRESS_NO_PORT: i64 = libc::IP_BIND_ADDRESS_NO_PORT as i64;
    pub(crate) const IPV6_MULTICAST_IF: i64 = libc::IPV6_MULTICAST_IF as i64;
    pub(crate) const IPV6_MULTICAST_HOPS: i64 = libc::IPV6_MULTICAST_HOPS as i64;
    pub(crate) const IPV6_MULTICAST_LOOP: i64 = libc::IPV6_MULTICAST_LOOP as i64;
    pub(crate) const IPV6_V6ONLY: i64 = libc::IPV6_V6ONLY as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const IP_PORTRANGE: i64 = 19;
    #[cfg(target_os = "macos")]
    pub(crate) const IP_PORTRANGE_DEFAULT: i64 = 0;
    #[cfg(target_os = "macos")]
    pub(crate) const IP_PORTRANGE_HIGH: i64 = 1;
    #[cfg(target_os = "macos")]
    pub(crate) const IP_PORTRANGE_LOW: i64 = 2;
    pub(crate) const SOCKET_EPERM: i64 = libc::EPERM as i64;
    pub(crate) const SOCKET_ENOENT: i64 = libc::ENOENT as i64;
    pub(crate) const SOCKET_EINTR: i64 = libc::EINTR as i64;
    pub(crate) const SOCKET_EIO: i64 = libc::EIO as i64;
    pub(crate) const SOCKET_ENXIO: i64 = libc::ENXIO as i64;
    pub(crate) const SOCKET_E2BIG: i64 = libc::E2BIG as i64;
    pub(crate) const SOCKET_EBADF: i64 = libc::EBADF as i64;
    pub(crate) const SOCKET_EAGAIN: i64 = libc::EAGAIN as i64;
    pub(crate) const SOCKET_ENOMEM: i64 = libc::ENOMEM as i64;
    pub(crate) const SOCKET_EACCES: i64 = libc::EACCES as i64;
    pub(crate) const SOCKET_EFAULT: i64 = libc::EFAULT as i64;
    pub(crate) const SOCKET_ENOTBLK: i64 = libc::ENOTBLK as i64;
    pub(crate) const SOCKET_EBUSY: i64 = libc::EBUSY as i64;
    pub(crate) const SOCKET_EEXIST: i64 = libc::EEXIST as i64;
    pub(crate) const SOCKET_EXDEV: i64 = libc::EXDEV as i64;
    pub(crate) const SOCKET_ENODEV: i64 = libc::ENODEV as i64;
    pub(crate) const SOCKET_ENOTDIR: i64 = libc::ENOTDIR as i64;
    pub(crate) const SOCKET_EISDIR: i64 = libc::EISDIR as i64;
    pub(crate) const SOCKET_EINVAL: i64 = libc::EINVAL as i64;
    pub(crate) const SOCKET_ENFILE: i64 = libc::ENFILE as i64;
    pub(crate) const SOCKET_EMFILE: i64 = libc::EMFILE as i64;
    pub(crate) const SOCKET_ENOTTY: i64 = libc::ENOTTY as i64;
    pub(crate) const SOCKET_ENOSPC: i64 = libc::ENOSPC as i64;
    pub(crate) const SOCKET_ESPIPE: i64 = libc::ESPIPE as i64;
    pub(crate) const SOCKET_EROFS: i64 = libc::EROFS as i64;
    pub(crate) const SOCKET_EMLINK: i64 = libc::EMLINK as i64;
    pub(crate) const SOCKET_EPIPE: i64 = libc::EPIPE as i64;
    pub(crate) const SOCKET_ENAMETOOLONG: i64 = libc::ENAMETOOLONG as i64;
    pub(crate) const SOCKET_ENOLCK: i64 = libc::ENOLCK as i64;
    pub(crate) const SOCKET_ENOSYS: i64 = libc::ENOSYS as i64;
    pub(crate) const SOCKET_ENOTEMPTY: i64 = libc::ENOTEMPTY as i64;
    pub(crate) const SOCKET_ELOOP: i64 = libc::ELOOP as i64;
    pub(crate) const SOCKET_EWOULDBLOCK: i64 = libc::EWOULDBLOCK as i64;
    pub(crate) const SOCKET_ENOMSG: i64 = libc::ENOMSG as i64;
    pub(crate) const SOCKET_EIDRM: i64 = libc::EIDRM as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ECHRNG: i64 = libc::ECHRNG as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EL2NSYNC: i64 = libc::EL2NSYNC as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EL3HLT: i64 = libc::EL3HLT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EL3RST: i64 = libc::EL3RST as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ELNRNG: i64 = libc::ELNRNG as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EUNATCH: i64 = libc::EUNATCH as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ENOCSI: i64 = libc::ENOCSI as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EL2HLT: i64 = libc::EL2HLT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EBADE: i64 = libc::EBADE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EBADR: i64 = libc::EBADR as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EXFULL: i64 = libc::EXFULL as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ENOANO: i64 = libc::ENOANO as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EBADRQC: i64 = libc::EBADRQC as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EBADSLT: i64 = libc::EBADSLT as i64;
    pub(crate) const SOCKET_ENOSTR: i64 = libc::ENOSTR as i64;
    pub(crate) const SOCKET_ENODATA: i64 = libc::ENODATA as i64;
    pub(crate) const SOCKET_ETIME: i64 = libc::ETIME as i64;
    pub(crate) const SOCKET_ENOSR: i64 = libc::ENOSR as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ENONET: i64 = libc::ENONET as i64;
    pub(crate) const SOCKET_EREMOTE: i64 = libc::EREMOTE as i64;
    pub(crate) const SOCKET_ENOLINK: i64 = libc::ENOLINK as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EADV: i64 = libc::EADV as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ESRMNT: i64 = libc::ESRMNT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ECOMM: i64 = libc::ECOMM as i64;
    pub(crate) const SOCKET_EPROTO: i64 = libc::EPROTO as i64;
    pub(crate) const SOCKET_EMULTIHOP: i64 = libc::EMULTIHOP as i64;
    pub(crate) const SOCKET_EBADMSG: i64 = libc::EBADMSG as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ENOTUNIQ: i64 = libc::ENOTUNIQ as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EBADFD: i64 = libc::EBADFD as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EREMCHG: i64 = libc::EREMCHG as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ERESTART: i64 = libc::ERESTART as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ESTRPIPE: i64 = libc::ESTRPIPE as i64;
    pub(crate) const SOCKET_EUSERS: i64 = libc::EUSERS as i64;
    pub(crate) const SOCKET_ENOTSOCK: i64 = libc::ENOTSOCK as i64;
    pub(crate) const SOCKET_EDESTADDRREQ: i64 = libc::EDESTADDRREQ as i64;
    pub(crate) const SOCKET_EMSGSIZE: i64 = libc::EMSGSIZE as i64;
    pub(crate) const SOCKET_EPROTOTYPE: i64 = libc::EPROTOTYPE as i64;
    pub(crate) const SOCKET_ENOPROTOOPT: i64 = libc::ENOPROTOOPT as i64;
    pub(crate) const SOCKET_EPROTONOSUPPORT: i64 = libc::EPROTONOSUPPORT as i64;
    pub(crate) const SOCKET_ESOCKTNOSUPPORT: i64 = libc::ESOCKTNOSUPPORT as i64;
    pub(crate) const SOCKET_EOPNOTSUPP: i64 = libc::EOPNOTSUPP as i64;
    pub(crate) const SOCKET_EPFNOSUPPORT: i64 = libc::EPFNOSUPPORT as i64;
    pub(crate) const SOCKET_EAFNOSUPPORT: i64 = libc::EAFNOSUPPORT as i64;
    pub(crate) const SOCKET_EADDRINUSE: i64 = libc::EADDRINUSE as i64;
    pub(crate) const SOCKET_EADDRNOTAVAIL: i64 = libc::EADDRNOTAVAIL as i64;
    pub(crate) const SOCKET_ENETDOWN: i64 = libc::ENETDOWN as i64;
    pub(crate) const SOCKET_ENETUNREACH: i64 = libc::ENETUNREACH as i64;
    pub(crate) const SOCKET_ENETRESET: i64 = libc::ENETRESET as i64;
    pub(crate) const SOCKET_ECONNABORTED: i64 = libc::ECONNABORTED as i64;
    pub(crate) const SOCKET_ECONNRESET: i64 = libc::ECONNRESET as i64;
    pub(crate) const SOCKET_ENOBUFS: i64 = libc::ENOBUFS as i64;
    pub(crate) const SOCKET_EISCONN: i64 = libc::EISCONN as i64;
    pub(crate) const SOCKET_ENOTCONN: i64 = libc::ENOTCONN as i64;
    pub(crate) const SOCKET_ESHUTDOWN: i64 = libc::ESHUTDOWN as i64;
    pub(crate) const SOCKET_ETOOMANYREFS: i64 = libc::ETOOMANYREFS as i64;
    pub(crate) const SOCKET_ETIMEDOUT: i64 = libc::ETIMEDOUT as i64;
    pub(crate) const SOCKET_ECONNREFUSED: i64 = libc::ECONNREFUSED as i64;
    pub(crate) const SOCKET_EHOSTDOWN: i64 = libc::EHOSTDOWN as i64;
    pub(crate) const SOCKET_EHOSTUNREACH: i64 = libc::EHOSTUNREACH as i64;
    pub(crate) const SOCKET_EALREADY: i64 = libc::EALREADY as i64;
    pub(crate) const SOCKET_EINPROGRESS: i64 = libc::EINPROGRESS as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EISNAM: i64 = libc::EISNAM as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EREMOTEIO: i64 = libc::EREMOTEIO as i64;
    pub(crate) const SOCKET_EDQUOT: i64 = libc::EDQUOT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_ENOMEDIUM: i64 = libc::ENOMEDIUM as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOCKET_EMEDIUMTYPE: i64 = libc::EMEDIUMTYPE as i64;
    pub(crate) const IPPROTO_IP: i64 = libc::IPPROTO_IP as i64;
    pub(crate) const IPPROTO_IPV6: i64 = libc::IPPROTO_IPV6 as i64;
    pub(crate) const SOL_TCP: i64 = libc::IPPROTO_TCP as i64;
    pub(crate) const SOL_UDP: i64 = libc::IPPROTO_UDP as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SOL_UDPLITE: i64 = libc::IPPROTO_UDPLITE as i64;
    pub(crate) const IPPROTO_ICMP: i64 = libc::IPPROTO_ICMP as i64;
    pub(crate) const IPPROTO_ICMPV6: i64 = libc::IPPROTO_ICMPV6 as i64;
    pub(crate) const IPV6_UNICAST_HOPS: i64 = libc::IPV6_UNICAST_HOPS as i64;
    pub(crate) const AI_PASSIVE: i64 = libc::AI_PASSIVE as i64;
    pub(crate) const AI_CANONNAME: i64 = libc::AI_CANONNAME as i64;
    pub(crate) const AI_NUMERICHOST: i64 = libc::AI_NUMERICHOST as i64;
    pub(crate) const AI_V4MAPPED: i64 = libc::AI_V4MAPPED as i64;
    pub(crate) const AI_ALL: i64 = libc::AI_ALL as i64;
    pub(crate) const AI_ADDRCONFIG: i64 = libc::AI_ADDRCONFIG as i64;
    /// glibc's `AI_IDN`, which the `libc` crate does not carry.
    #[cfg(target_os = "linux")]
    pub(crate) const AI_IDN: i64 = 0x40;
    /// glibc's `AI_CANONIDN`, which the `libc` crate does not carry.
    #[cfg(target_os = "linux")]
    pub(crate) const AI_CANONIDN: i64 = 0x80;
    pub(crate) const AI_NUMERICSERV: i64 = libc::AI_NUMERICSERV as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const SOL_LOCAL: i64 = libc::SOL_LOCAL as i64;
    pub(crate) const IPV6_RECVPKTINFO: i64 = libc::IPV6_RECVPKTINFO as i64;
    pub(crate) const IPV6_PKTINFO: i64 = libc::IPV6_PKTINFO as i64;
    pub(crate) const IPV6_RECVHOPLIMIT: i64 = libc::IPV6_RECVHOPLIMIT as i64;
    pub(crate) const IPV6_HOPLIMIT: i64 = libc::IPV6_HOPLIMIT as i64;
    pub(crate) const IPV6_RECVTCLASS: i64 = libc::IPV6_RECVTCLASS as i64;
    pub(crate) const IPV6_TCLASS: i64 = libc::IPV6_TCLASS as i64;
    pub(crate) const SCM_RIGHTS: i64 = libc::SCM_RIGHTS as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SCM_CREDENTIALS: i64 = libc::SCM_CREDENTIALS as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_PASSCRED: i64 = libc::SO_PASSCRED as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_ATTACH_REUSEPORT_CBPF: i64 = libc::SO_ATTACH_REUSEPORT_CBPF as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_DETACH_FILTER: i64 = libc::SO_DETACH_FILTER as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const SO_DETACH_BPF: i64 = libc::SO_DETACH_BPF as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const SO_NOSIGPIPE: i64 = libc::SO_NOSIGPIPE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_QUICKACK: i64 = libc::TCP_QUICKACK as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const TCP_REPAIR: i64 = libc::TCP_REPAIR as i64;
    #[cfg(target_os = "macos")]
    pub(crate) const IP_DONTFRAG: i64 = libc::IP_DONTFRAG as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const IP_MTU_DISCOVER: i64 = libc::IP_MTU_DISCOVER as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const IP_PMTUDISC_DO: i64 = libc::IP_PMTUDISC_DO as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const IP_PMTUDISC_DONT: i64 = libc::IP_PMTUDISC_DONT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const IP_PMTUDISC_WANT: i64 = libc::IP_PMTUDISC_WANT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const IP_PMTUDISC_PROBE: i64 = libc::IP_PMTUDISC_PROBE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const IP_PMTUDISC_INTERFACE: i64 = libc::IP_PMTUDISC_INTERFACE as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const IP_PMTUDISC_OMIT: i64 = libc::IP_PMTUDISC_OMIT as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const ETH_P_IP: i64 = libc::ETH_P_IP as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const ETH_P_IPV6: i64 = libc::ETH_P_IPV6 as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const ETH_P_LOOP: i64 = libc::ETH_P_LOOP as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const ETH_P_ALL: i64 = libc::ETH_P_ALL as i64;
    #[cfg(target_os = "linux")]
    pub(crate) const UDP_SEGMENT: i64 = libc::UDP_SEGMENT as i64;
    pub(crate) const SHUT_RD: i64 = libc::SHUT_RD as i64;
    pub(crate) const SHUT_WR: i64 = libc::SHUT_WR as i64;
    pub(crate) const SHUT_RDWR: i64 = libc::SHUT_RDWR as i64;
}

/// The extension value (design §7.1): declarations paired with entries,
/// the constants, the two classes and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('sockets'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::declared())
        .functions(&[
            natives::socket_select::DECLARED,
            natives::socket_create_listen::DECLARED,
            natives::socket_accept::DECLARED,
            natives::socket_set_nonblock::DECLARED,
            natives::socket_set_block::DECLARED,
            natives::socket_listen::DECLARED,
            natives::socket_close::DECLARED,
            natives::socket_write::DECLARED,
            natives::socket_read::DECLARED,
            natives::socket_getsockname::DECLARED,
            natives::socket_getpeername::DECLARED,
            natives::socket_create::DECLARED,
            natives::socket_connect::DECLARED,
            natives::socket_strerror::DECLARED,
            natives::socket_bind::DECLARED,
            natives::socket_recv::DECLARED,
            natives::socket_send::DECLARED,
            natives::socket_recvfrom::DECLARED,
            natives::socket_sendto::DECLARED,
            options::socket_get_option::DECLARED,
            options::socket_getopt::DECLARED,
            options::socket_set_option::DECLARED,
            options::socket_setopt::DECLARED,
            natives::socket_create_pair::DECLARED,
            natives::socket_shutdown::DECLARED,
            natives::socket_atmark::DECLARED,
            natives::socket_last_error::DECLARED,
            natives::socket_clear_error::DECLARED,
            stream::socket_import_stream::DECLARED,
            stream::socket_export_stream::DECLARED,
            msg::socket_sendmsg::DECLARED,
            msg::socket_recvmsg::DECLARED,
            msg::socket_cmsg_space::DECLARED,
            addrinfo::socket_addrinfo_lookup::DECLARED,
            addrinfo::socket_addrinfo_connect::DECLARED,
            addrinfo::socket_addrinfo_bind::DECLARED,
            addrinfo::socket_addrinfo_explain::DECLARED,
        ])
        .class::<socket::Socket>()
        .class::<addrinfo::AddressInfo>()
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .info(sockets_info)
        .build("sockets")
}

/// `PHP_MINFO_FUNCTION(sockets)`: the one-row table.
///
/// ```php
/// (new ReflectionExtension('sockets'))->info();
/// ```
/// ```expect
/// sockets
///
/// Sockets Support => enabled
/// ```
fn sockets_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["Sockets Support", "enabled"]]);
}
