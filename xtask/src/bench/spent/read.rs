//! The reader behind [`Retired::of`](super::Retired::of) on macOS, xtask's
//! one `unsafe` home (`unsafe.homes`, #3062's ruling): what a process and
//! its children had retired, from `proc_pid_rusage(RUSAGE_INFO_V4)` over
//! the process and each pid `proc_listchildpids` names, summed. Apple
//! silicon states user, system and runnable time in mach ticks, so each is
//! taken to nanoseconds through `mach_timebase_info`; on Intel the
//! timebase is one, and the conversion is the identity.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use nix::libc;

/// The most children of one server read: php-fpm's pool of eight, with
/// room to spare.
const CHILDREN: usize = 64;

/// What `pid` and its children had retired, summed: instructions, cycles,
/// CPU (user and system) and runnable nanoseconds. None where `pid` itself
/// or its children cannot be read; a child that ended between the listing
/// and its read is left out, and the total that went back says so.
pub(super) fn totals(pid: u32) -> Option<[u64; 4]> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    let mut children: [libc::pid_t; CHILDREN] = [0; CHILDREN];
    let size = libc::c_int::try_from(std::mem::size_of_val(&children)).ok()?;
    // SAFETY: `children` is `size` writable bytes, which the call fills
    // with at most that many bytes of pids.
    let listed = unsafe { libc::proc_listchildpids(pid, children.as_mut_ptr().cast(), size) };
    let listed = usize::try_from(listed).ok()?.min(CHILDREN);
    let (numer, denom) = timebase()?;
    let ns = |ticks: u64| ticks.saturating_mul(u64::from(numer)) / u64::from(denom);
    let mut sum = [0_u64; 4];
    let read = children[..listed].iter().filter_map(|&child| usage(child));
    for info in std::iter::once(usage(pid)?).chain(read) {
        let cpu = info.ri_user_time.saturating_add(info.ri_system_time);
        let figures = [
            info.ri_instructions,
            info.ri_cycles,
            ns(cpu),
            ns(info.ri_runnable_time),
        ];
        for (total, figure) in sum.iter_mut().zip(figures) {
            *total = total.saturating_add(figure);
        }
    }
    Some(sum)
}

/// The process `pid`'s `rusage_info_v4`, or none where it cannot be read.
fn usage(pid: libc::pid_t) -> Option<libc::rusage_info_v4> {
    // SAFETY: `rusage_info_v4` is integers and bytes, for which every bit
    // pattern, zero among them, is a value.
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is the `rusage_info_v4` the flavor `RUSAGE_INFO_V4`
    // writes whole, and outlives the call.
    let read = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
    (read == 0).then_some(info)
}

/// The mach timebase, a tick's nanoseconds as `numer / denom`.
#[allow(
    deprecated,
    reason = "libc points at mach2, which the lock does not hold"
)]
fn timebase() -> Option<(u32, u32)> {
    let mut base = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: `base` is the out-parameter the call writes, and outlives it.
    let status = unsafe { libc::mach_timebase_info(&raw mut base) };
    (status == 0 && base.denom != 0).then_some((base.numer, base.denom))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    /// A thread that spins for 100 ms is running or waiting for a CPU the
    /// whole time, so its process's CPU and runnable time grow by about
    /// that much: at least half of it, where ticks taken for nanoseconds
    /// would be a fortieth on Apple silicon. Its instructions and cycles
    /// grow, and a pid no process has reads none.
    #[test]
    fn a_spinning_thread_s_time_is_read_in_nanoseconds() {
        let pid = std::process::id();
        let before = super::totals(pid).unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(100) {
            std::hint::black_box(());
        }
        let after = super::totals(pid).unwrap();
        let grew: Vec<u64> = (after.iter().zip(before))
            .map(|(a, b)| a.saturating_sub(b))
            .collect();
        assert!(grew[0] > 0 && grew[1] > 0, "{grew:?}");
        assert!(grew[2] + grew[3] >= 50_000_000, "{grew:?}");
        assert_eq!(super::totals(u32::MAX >> 1), None);
    }
}
