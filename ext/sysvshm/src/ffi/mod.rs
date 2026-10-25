//! System V shared memory, `<sys/shm.h>` from the C library (design §7.2;
//! `unsafe.homes`' `ext/*/src/ffi/`), since nix binds no System V IPC
//! (#1323): `shmget`, `shmctl` and `shmat` behind safe functions, and the
//! attached segment as a value that detaches when it is dropped, read and
//! written only within the size `IPC_STAT` answers, whatever the bytes
//! another process wrote there claim. The one home of this crate's
//! `unsafe`.
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
    if id < 0 { Err(errno()) } else { Ok(id) }
}

/// `shmctl(id, IPC_RMID, NULL)`: the segment marked for deletion, or the
/// errno.
pub(crate) fn remove(id: i32) -> Result<(), i32> {
    // SAFETY: `IPC_RMID` reads no buffer, so the null one is allowed.
    if unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) } < 0 {
        return Err(errno());
    }
    Ok(())
}

/// One attachment of a segment: `shmat`'s address and the size
/// `IPC_STAT` answers; `shmdt` when it is dropped, as `shm_detach` and
/// `sysvshm_free_obj` detach.
pub(crate) struct Segment {
    addr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping belongs to the process, not to the thread that
// attached it; any thread may read, write and detach it.
unsafe impl Send for Segment {}

impl Segment {
    /// `shmat(id, NULL, 0)`, then its size read with `IPC_STAT`, in
    /// php's order, or the errno of the first that failed; a segment
    /// whose `IPC_STAT` failed is detached again.
    pub(crate) fn attach(id: i32) -> Result<Segment, i32> {
        // SAFETY: a null address lets the kernel choose where the segment
        // is mapped, so nothing of ours is overlaid.
        let addr = unsafe { libc::shmat(id, std::ptr::null(), 0) };
        if addr as isize == -1 {
            return Err(errno());
        }
        let addr = NonNull::new(addr.cast::<u8>()).ok_or_else(errno)?;
        let mut segment = Segment { addr, len: 0 };
        // SAFETY: `shmid_ds` is integers and a permission struct of
        // integers, for which all zeroes is a value.
        let mut shm: libc::shmid_ds = unsafe { std::mem::zeroed() };
        // SAFETY: `shm` is a `shmid_ds` this frame owns, which `IPC_STAT`
        // writes and nothing else.
        if unsafe { libc::shmctl(id, libc::IPC_STAT, &raw mut shm) } != 0 {
            // `segment` drops here, which is php's `shmdt(shm_ptr)`.
            return Err(errno());
        }
        segment.len = shm.shm_segsz;
        Ok(segment)
    }

    /// The segment's size, `shm_segsz` as `IPC_STAT` answered it.
    pub(crate) fn size(&self) -> usize {
        self.len
    }

    /// Whether `n` bytes from `at` lie within the segment.
    fn holds(&self, at: usize, n: usize) -> bool {
        at.checked_add(n).is_some_and(|end| end <= self.len)
    }

    /// A copy of `n` bytes from `at`, `None` where they leave the segment.
    pub(crate) fn read(&self, at: usize, n: usize) -> Option<Vec<u8>> {
        if !self.holds(at, n) {
            return None;
        }
        let mut out = vec![0; n];
        // SAFETY: `at + n` lies within the `len` bytes `shmat` mapped, and
        // `out` is `n` bytes of our own.
        unsafe {
            std::ptr::copy_nonoverlapping(self.addr.as_ptr().wrapping_add(at), out.as_mut_ptr(), n);
        }
        Some(out)
    }

    /// `data` written at `at`: whether it lay within the segment.
    pub(crate) fn write(&self, at: usize, data: &[u8]) -> bool {
        if !self.holds(at, data.len()) {
            return false;
        }
        // SAFETY: `at + data.len()` lies within the `len` bytes `shmat`
        // mapped writable, and `data` is that many bytes of our own.
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.addr.as_ptr().wrapping_add(at),
                data.len(),
            );
        }
        true
    }

    /// `memmove(at + to, at + from, n)`: whether both ranges lay within
    /// the segment.
    pub(crate) fn shift(&self, from: usize, to: usize, n: usize) -> bool {
        if !self.holds(from, n) || !self.holds(to, n) {
            return false;
        }
        let base = self.addr.as_ptr();
        // SAFETY: both ranges lie within the `len` bytes `shmat` mapped,
        // and `copy` allows them to overlap.
        unsafe { std::ptr::copy(base.wrapping_add(from), base.wrapping_add(to), n) };
        true
    }

    /// The native-endian word at `at`, a `size_t` or `zend_long` of php's
    /// layout.
    pub(crate) fn word(&self, at: usize) -> Option<u64> {
        let bytes = self.read(at, 8)?;
        Some(u64::from_ne_bytes(bytes.try_into().ok()?))
    }

    /// The word `value` written at `at`.
    pub(crate) fn set_word(&self, at: usize, value: u64) -> bool {
        self.write(at, &value.to_ne_bytes())
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: `addr` is the address `shmat` answered, attached once
        // and detached here once.
        unsafe { libc::shmdt(self.addr.as_ptr().cast()) };
    }
}
