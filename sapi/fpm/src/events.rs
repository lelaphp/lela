//! `fpm_events.c`'s module: the descriptors the master waits on, as php
//! picks the mechanism — kqueue where the system has it, epoll on Linux —
//! each registered read, edge-triggered where the event asks
//! (`FPM_EV_EDGE`, the ondemand listening socket), and the time a wait
//! may take.

use std::os::fd::{BorrowedFd, RawFd};

/// `module->name`, what `-tt` prints as `events.mechanism`.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
pub(crate) const MECHANISM: &str = "kqueue";
#[cfg(target_os = "linux")]
pub(crate) const MECHANISM: &str = "epoll";

/// The master's queue of descriptors, `kfd` or `epollfd`.
pub(crate) struct Poller {
    #[cfg(not(target_os = "linux"))]
    kq: nix::sys::event::Kqueue,
    #[cfg(target_os = "linux")]
    ep: nix::sys::epoll::Epoll,
}

#[cfg(not(target_os = "linux"))]
impl Poller {
    /// `fpm_event_kqueue_init`.
    pub(crate) fn new() -> nix::Result<Poller> {
        Ok(Poller {
            kq: nix::sys::event::Kqueue::new()?,
        })
    }

    fn change(&self, fd: RawFd, flags: nix::sys::event::EvFlags) -> nix::Result<()> {
        use nix::sys::event::{EventFilter, FilterFlag, KEvent};
        let ident = usize::try_from(fd).unwrap_or(0);
        let event = KEvent::new(
            ident,
            EventFilter::EVFILT_READ,
            flags,
            FilterFlag::empty(),
            0,
            0,
        );
        self.kq.kevent(&[event], &mut [], None).map(|_| ())
    }

    /// `fpm_event_kqueue_add`.
    pub(crate) fn add(&self, fd: BorrowedFd<'_>, edge: bool) -> nix::Result<()> {
        use nix::sys::event::EvFlags;
        use std::os::fd::AsRawFd;
        let flags = if edge {
            EvFlags::EV_ADD | EvFlags::EV_CLEAR
        } else {
            EvFlags::EV_ADD
        };
        self.change(fd.as_raw_fd(), flags)
    }

    /// `fpm_event_kqueue_remove`.
    pub(crate) fn remove(&self, fd: BorrowedFd<'_>) {
        use std::os::fd::AsRawFd;
        let _ = self.change(fd.as_raw_fd(), nix::sys::event::EvFlags::EV_DELETE);
    }

    /// `fpm_event_kqueue_wait`: the descriptors ready within `timeout_ms`,
    /// none where a signal cut the wait short.
    pub(crate) fn wait(&self, timeout_ms: u64) -> Vec<RawFd> {
        use nix::sys::event::KEvent;
        let mut events = vec![
            KEvent::new(
                0,
                nix::sys::event::EventFilter::EVFILT_READ,
                nix::sys::event::EvFlags::empty(),
                nix::sys::event::FilterFlag::empty(),
                0,
                0
            );
            64
        ];
        let timeout = nix::libc::timespec {
            tv_sec: i64::try_from(timeout_ms / 1000).unwrap_or(0) as nix::libc::time_t,
            tv_nsec: i64::try_from((timeout_ms % 1000) * 1_000_000).unwrap_or(0)
                as nix::libc::c_long,
        };
        match self.kq.kevent(&[], &mut events, Some(timeout)) {
            Ok(n) => events[..n]
                .iter()
                .map(|event| RawFd::try_from(event.ident()).unwrap_or(-1))
                .collect(),
            Err(nix::errno::Errno::EINTR) => Vec::new(),
            Err(e) => {
                crate::zlog!(Warning @ fpm_event_kqueue_wait:131, "kevent() returns {}", e as i32);
                Vec::new()
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Poller {
    /// `fpm_event_epoll_init`.
    pub(crate) fn new() -> nix::Result<Poller> {
        use nix::sys::epoll::{Epoll, EpollCreateFlags};
        Ok(Poller {
            ep: Epoll::new(EpollCreateFlags::empty())?,
        })
    }

    /// `fpm_event_epoll_add`.
    pub(crate) fn add(&self, fd: BorrowedFd<'_>, edge: bool) -> nix::Result<()> {
        use nix::sys::epoll::{EpollEvent, EpollFlags};
        use std::os::fd::AsRawFd;
        let flags = if edge {
            EpollFlags::EPOLLIN | EpollFlags::EPOLLET
        } else {
            EpollFlags::EPOLLIN
        };
        let token = u64::try_from(fd.as_raw_fd()).unwrap_or(0);
        self.ep.add(fd, EpollEvent::new(flags, token))
    }

    /// `fpm_event_epoll_remove`.
    pub(crate) fn remove(&self, fd: BorrowedFd<'_>) {
        let _ = self.ep.delete(fd);
    }

    /// `fpm_event_epoll_wait`.
    pub(crate) fn wait(&self, timeout_ms: u64) -> Vec<RawFd> {
        use nix::sys::epoll::{EpollEvent, EpollTimeout};
        let mut events = [EpollEvent::empty(); 64];
        let timeout = EpollTimeout::try_from(timeout_ms).unwrap_or(EpollTimeout::NONE);
        match self.ep.wait(&mut events, timeout) {
            Ok(n) => events[..n]
                .iter()
                .map(|event| RawFd::try_from(event.data()).unwrap_or(-1))
                .collect(),
            Err(nix::errno::Errno::EINTR) => Vec::new(),
            Err(e) => {
                crate::zlog!(Warning @ fpm_event_epoll_wait:127, "epoll_wait() returns {}", e as i32);
                Vec::new()
            }
        }
    }
}
