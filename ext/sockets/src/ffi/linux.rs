//! The calls php makes under Linux's `#ifdef`s: `accept4(2)`
//! (`HAVE_ACCEPT4`), an `AF_PACKET` socket's `sockaddr_ll`, the classic
//! BPF program `SO_ATTACH_REUSEPORT_CBPF` attaches, and RFC 3678's
//! `group_req` and `group_source_req`, which the `libc` crate does not
//! declare.

use std::mem::size_of;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use nix::errno::Errno;

use super::{Addr, encode, len_of, result, storage};

/// `accept4(fd, &la, &la_len, flags)`, `php_accept_connect` where the
/// platform has it: the new fd made with `flags`, and `la->sa_family`.
pub(crate) fn accept4(fd: RawFd, flags: i32) -> Result<(OwnedFd, i32), Errno> {
    let mut ss = storage();
    let mut len = len_of::<libc::sockaddr_storage>();
    // SAFETY: `ss` is a live `sockaddr_storage` of `len` bytes, which the
    // call writes the peer's address into.
    let new = result(unsafe { libc::accept4(fd, (&raw mut ss).cast(), &raw mut len, flags) })?;
    // SAFETY: `accept4` answered a new descriptor nothing else owns.
    let owned = unsafe { OwnedFd::from_raw_fd(new) };
    Ok((owned, i32::from(ss.ss_family)))
}

/// `getsockname(2)` of an `AF_PACKET` socket: its `sll_ifindex`.
pub(crate) fn packet_index(fd: RawFd) -> Result<i32, Errno> {
    let mut ss = storage();
    let mut len = len_of::<libc::sockaddr_storage>();
    // SAFETY: `ss` is a live `sockaddr_storage` of `len` bytes.
    result(unsafe { libc::getsockname(fd, (&raw mut ss).cast(), &raw mut len) })?;
    // SAFETY: the storage is aligned for every `sockaddr` and was zeroed,
    // so it reads as a `sockaddr_ll` whatever the call wrote.
    let sll = unsafe { std::ptr::read((&raw const ss).cast::<libc::sockaddr_ll>()) };
    Ok(sll.sll_ifindex)
}

/// Why `socket_bind` of an `AF_PACKET` socket failed.
pub(crate) enum PacketBind {
    /// `getsockname(2)` refused: php's `invalid AF_PACKET socket`.
    Invalid,
    /// `bind(2)` refused.
    Bind(Errno),
}

/// `socket_bind`'s `AF_PACKET` arm as php writes it: `getsockname(2)`
/// into a zeroed storage with a length of `sizeof(sa)`, which is the
/// size of the pointer php declared and so reads the family, the
/// protocol and the index; `sll_ifindex` set; and `bind(2)` with the
/// size of a whole `sockaddr_ll`.
pub(crate) fn bind_packet(fd: RawFd, ifindex: u32) -> Result<(), PacketBind> {
    let mut ss = storage();
    let mut len = len_of::<*const libc::sockaddr_ll>();
    // SAFETY: `ss` is a live `sockaddr_storage` of more than `len` bytes.
    let named = unsafe { libc::getsockname(fd, (&raw mut ss).cast(), &raw mut len) };
    if named < 0 {
        return Err(PacketBind::Invalid);
    }
    // SAFETY: the storage is aligned for every `sockaddr` and was zeroed.
    let mut sll = unsafe { std::ptr::read((&raw const ss).cast::<libc::sockaddr_ll>()) };
    sll.sll_ifindex = i32::from_ne_bytes(ifindex.to_ne_bytes());
    // SAFETY: `sll` is a live `sockaddr_ll` of the length given, read and
    // not kept.
    let bound = unsafe { libc::bind(fd, (&raw const sll).cast(), len_of::<libc::sockaddr_ll>()) };
    result(bound).map(drop).map_err(PacketBind::Bind)
}

/// `setsockopt(fd, level, name, &value, sizeof(value))`.
fn set<T>(fd: RawFd, level: i32, name: i32, value: &T) -> Result<(), Errno> {
    // SAFETY: `value` is a live `T` of the length given, read and not kept.
    let set = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            std::ptr::from_ref(value).cast(),
            len_of::<T>(),
        )
    };
    result(set).map(drop)
}

/// `SO_ATTACH_REUSEPORT_CBPF` with the program php builds for
/// `SKF_AD_CPU` or `SKF_AD_QUEUE`: load the ancillary word `k` names and
/// return it, so the kernel picks the socket of that CPU or queue. The
/// kernel copies the program before the call returns.
pub(crate) fn attach_cbpf(fd: RawFd, level: i32, k: u32) -> Result<(), Errno> {
    let none = libc::sock_filter {
        code: 0,
        jt: 0,
        jf: 0,
        k: 0,
    };
    let mut filter = [none; 8];
    filter[0] = libc::sock_filter {
        code: u16::try_from(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS).unwrap_or(0),
        jt: 0,
        jf: 0,
        k: k.wrapping_add(libc::SKF_AD_OFF.cast_unsigned()),
    };
    filter[1].code = u16::try_from(libc::BPF_RET | libc::BPF_A).unwrap_or(0);
    let program = libc::sock_fprog {
        len: 2,
        filter: filter.as_mut_ptr(),
    };
    set(fd, level, libc::SO_ATTACH_REUSEPORT_CBPF, &program)
}

/// RFC 3678's `struct group_req`.
#[repr(C)]
struct GroupReq {
    interface: u32,
    group: libc::sockaddr_storage,
}

/// RFC 3678's `struct group_source_req`.
#[repr(C)]
struct GroupSourceReq {
    interface: u32,
    group: libc::sockaddr_storage,
    source: libc::sockaddr_storage,
}

/// `_php_mcast_join_leave` and `_php_mcast_source_op` under
/// `RFC3678_API`: `option` set with the group, and the source where there
/// is one, each copied into a zeroed `sockaddr_storage` at its family's
/// length, and the interface's index.
pub(crate) fn mcast_group(
    fd: RawFd,
    level: i32,
    option: i32,
    interface: u32,
    group: &Addr,
    source: Option<&Addr>,
) -> Result<(), Errno> {
    let (group, _) = encode(group);
    match source {
        None => set(fd, level, option, &GroupReq { interface, group }),
        Some(source) => {
            let (source, _) = encode(source);
            let request = GroupSourceReq {
                interface,
                group,
                source,
            };
            set(fd, level, option, &request)
        }
    }
}

/// `sizeof(struct group_req)` and `sizeof(struct group_source_req)` as
/// glibc lays them out on `x86_64` and `aarch64`.
const _: () = assert!(size_of::<GroupReq>() == 136 && size_of::<GroupSourceReq>() == 264);
