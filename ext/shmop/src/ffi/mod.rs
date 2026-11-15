//! System V shared memory, `<sys/shm.h>` from the C library (design §7.2;
//! `unsafe.homes`' `ext/*/src/ffi/`), since nix binds no System V IPC:
//! `shmget`, `shmctl` and `shmat` behind safe functions, and the
//! attached mapping as a value that detaches when it is dropped. The one
//! home of this crate's `unsafe`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ptr::NonNull;

/// The `errno` of the call that just failed.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `shmget(key, size, flags)`: the segment's id, or the errno.
pub(crate) fn get(key: i32, size: usize, flags: i32) -> Result<i32, i32> {
    // SAFETY: `shmget` takes three values and touches no memory of ours.
    let id = unsafe { libc::shmget(key, size, flags) };
    if id == -1 { Err(errno()) } else { Ok(id) }
}

/// `shmctl(id, IPC_STAT, &shm)`'s `shm_segsz`: the segment's size in
/// bytes, or the errno.
pub(crate) fn size(id: i32) -> Result<usize, i32> {
    // SAFETY: `shmid_ds` is integers and a permission struct of integers,
    // for which all zeroes is a value.
    let mut shm: libc::shmid_ds = unsafe { std::mem::zeroed() };
    // SAFETY: `shm` is a `shmid_ds` this frame owns, which `IPC_STAT`
    // writes and nothing else.
    if unsafe { libc::shmctl(id, libc::IPC_STAT, &raw mut shm) } != 0 {
        return Err(errno());
    }
    Ok(shm.shm_segsz)
}

/// One attachment of a segment, php's `php_shmop` less its `key` and
/// flags: the id, `shmat`'s address, the size `IPC_STAT` answered and
/// whether it was attached `SHM_RDONLY`; `shmdt` when it is dropped, as
/// `shmop_free_obj` detaches.
pub(crate) struct Segment {
    addr: NonNull<u8>,
    len: usize,
    id: i32,
    read_only: bool,
}

// SAFETY: the mapping belongs to the process, not to the thread that
// attached it; any thread may read, write and detach it.
unsafe impl Send for Segment {}

impl Segment {
    /// `shmat(id, 0, read_only ? SHM_RDONLY : 0)` over a segment of `len`
    /// bytes, or the errno.
    pub(crate) fn attach(id: i32, read_only: bool, len: usize) -> Result<Segment, i32> {
        let flags = if read_only { libc::SHM_RDONLY } else { 0 };
        // SAFETY: a null address lets the kernel choose where the segment
        // is mapped, so nothing of ours is overlaid.
        let addr = unsafe { libc::shmat(id, std::ptr::null(), flags) };
        if addr as isize == -1 {
            return Err(errno());
        }
        let addr = NonNull::new(addr.cast::<u8>()).ok_or_else(errno)?;
        Ok(Segment {
            addr,
            len,
            id,
            read_only,
        })
    }

    /// The segment's size in bytes.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Whether it was attached `SHM_RDONLY`.
    pub(crate) const fn read_only(&self) -> bool {
        self.read_only
    }

    /// A copy of `count` bytes from `start`, as `zend_string_init` copies
    /// them; empty where the range leaves the segment.
    pub(crate) fn read(&self, start: usize, count: usize) -> Vec<u8> {
        if start.checked_add(count).is_none_or(|end| end > self.len) {
            return Vec::new();
        }
        let mut out = vec![0; count];
        // SAFETY: `start + count` lies within the `len` bytes `shmat`
        // mapped, and `out` is `count` bytes of our own.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.addr.as_ptr().wrapping_add(start),
                out.as_mut_ptr(),
                count,
            );
        }
        out
    }

    /// `memcpy(addr + offset, data, n)` of as much of `data` as fits
    /// before the segment's end: the count written, none to a segment
    /// attached read-only.
    pub(crate) fn write(&self, offset: usize, data: &[u8]) -> usize {
        let n = data.len().min(self.len.saturating_sub(offset));
        if n == 0 || self.read_only {
            return 0;
        }
        // SAFETY: `offset + n` lies within the `len` bytes `shmat` mapped
        // writable, and `data` is at least `n` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.addr.as_ptr().wrapping_add(offset),
                n,
            );
        }
        n
    }

    /// `shmctl(id, IPC_RMID, NULL)`: the segment marked for deletion, or
    /// the errno.
    pub(crate) fn remove(&self) -> Result<(), i32> {
        // SAFETY: `IPC_RMID` reads no buffer, so the null one is allowed.
        if unsafe { libc::shmctl(self.id, libc::IPC_RMID, std::ptr::null_mut()) } != 0 {
            return Err(errno());
        }
        Ok(())
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: `addr` is the address `shmat` answered, attached once
        // and detached here once.
        unsafe { libc::shmdt(self.addr.as_ptr().cast()) };
    }
}
