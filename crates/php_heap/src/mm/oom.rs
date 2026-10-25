//! The memory limit as php 8.5.12 holds a request to it (CVE-2026-109618,
//! #3260): `Z/Zend/zend_alloc.c`'s `zend_mm_check_limit` and its two
//! thresholds. An allocation past `memory_limit`, the soft limit, is
//! served and leaves a delayed OOM behind, php's `EG(delayed_oom)`, which
//! the engine raises as the limit's fatal at its next safepoint; one past
//! the hard limit ends the process where it stands. This file holds
//! the record and the hard limit's arithmetic; [`super::Mm`] holds the
//! request to both.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::NO_LIMIT;

/// php's `EG(delayed_oom)`, a `zend_oom_info` (`Z/Zend/zend_alloc.h:46`):
/// where the request stands with the limit's fatal, its
/// `ZEND_OOM_STATE_*` with what the pending state carries. Anything but
/// [`DelayedOom::None`] is php's `ZEND_HAS_OOM()`, under which a native
/// raises no diagnostic, a destructor is not served and a backtrace
/// stops at its first frame, until `php_request_shutdown` makes it
/// `None` again.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum DelayedOom {
    /// `ZEND_OOM_STATE_NONE`: the limit has not been met.
    #[default]
    None,
    /// `ZEND_OOM_STATE_PENDING`: met, and the fatal not yet raised.
    Pending(PendingOom),
    /// `ZEND_OOM_STATE_HANDLING`: the fatal being raised,
    /// `zend_handle_delayed_oom` under way.
    Handling,
    /// `ZEND_OOM_STATE_HANDLED`: raised, the request unwinding or past
    /// it.
    Handled,
}

/// What `zend_mm_delayed_verror` records: the fatal's message, and the
/// site php reads as it records it, `zend_get_executed_filename_ex()` and
/// `zend_get_executed_lineno()`, which the engine names
/// ([`crate::Heap::locate_oom`]), since the heap cannot read a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOom {
    /// php's `info->message`: `Allowed memory size of %zu bytes
    /// exhausted (tried to allocate %zu bytes)`.
    pub message: String,
    /// php's `info->filename` and `info->lineno`, once the engine named
    /// them.
    pub site: Option<(Box<[u8]>, u32)>,
}

/// php's `heap->hard_limit_percent`, what `memory_hard_limit_percent`
/// sets (`Z/main/main.c:391`): php's `-1`, its default, or a percent of
/// the soft limit no lower than a hundred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HardLimitPercent {
    /// `-1`: twice the soft limit, or the soft limit and 1 GiB above a
    /// soft limit of more than 1 GiB.
    Default,
    /// A percent of the soft limit, 100 or more.
    Of(u64),
}

impl HardLimitPercent {
    /// php's `zend_long` read as one: `-1`, or 100 and above; anything
    /// else is php's `FAILURE`.
    pub(crate) fn from_php(percent: i64) -> Option<HardLimitPercent> {
        match percent {
            -1 => Some(HardLimitPercent::Default),
            100.. => u64::try_from(percent).ok().map(HardLimitPercent::Of),
            _ => None,
        }
    }

    /// `zend_mm_update_hard_limit` (`Z/Zend/zend_alloc.c:689`): the hard
    /// limit a soft limit of `limit` bytes takes, never past php's
    /// `ZEND_MM_LIMIT_MAX`.
    pub(crate) fn hard_limit(self, limit: usize) -> usize {
        const GIB: usize = 1024 * 1024 * 1024;
        let percent = match self {
            HardLimitPercent::Default if limit > GIB => {
                return limit
                    .checked_add(GIB)
                    .map_or(NO_LIMIT, |hard| hard.min(NO_LIMIT));
            }
            HardLimitPercent::Default => 200,
            HardLimitPercent::Of(percent) => percent,
        };
        scale_by_percent(limit, percent).min(NO_LIMIT)
    }
}

/// php's `scale_by_percent` (`Z/Zend/zend_alloc.c:666`): `value` times
/// `percent` over a hundred, rounded down, computed without overflowing
/// and `usize::MAX` where the answer would.
fn scale_by_percent(value: usize, percent: u64) -> usize {
    let Ok(p) = usize::try_from(percent) else {
        return usize::MAX;
    };
    let (q, r) = (value / 100, value % 100);
    let Some(hi) = q.checked_mul(p) else {
        return usize::MAX;
    };
    let lo = r * (p / 100) + r * (p % 100) / 100;
    hi.saturating_add(lo)
}

/// The word the heap raises as it records a delayed OOM, php's
/// `zend_atomic_bool_store_ex(&EG(vm_interrupt), true)`: the request's
/// interrupt, which the engine hands the heap as the request starts
/// ([`crate::Heap::wake_on_oom`]).
pub(crate) type Wake = Arc<dyn AsRef<AtomicBool> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hard_limit_is_php_s() {
        let mib = 1024 * 1024;
        assert_eq!(HardLimitPercent::Default.hard_limit(2 * mib), 4 * mib);
        assert_eq!(HardLimitPercent::Default.hard_limit(128 * mib), 256 * mib);
        assert_eq!(HardLimitPercent::Default.hard_limit(2048 * mib), 3072 * mib);
        assert_eq!(HardLimitPercent::Default.hard_limit(NO_LIMIT), NO_LIMIT);
        assert_eq!(HardLimitPercent::Of(150).hard_limit(8 * mib), 12 * mib);
        assert_eq!(HardLimitPercent::Of(100).hard_limit(999), 999);
        assert_eq!(HardLimitPercent::Of(101).hard_limit(199), 200);
        assert_eq!(HardLimitPercent::from_php(99), None);
        assert_eq!(HardLimitPercent::from_php(-2), None);
        assert_eq!(
            HardLimitPercent::from_php(-1),
            Some(HardLimitPercent::Default)
        );
    }
}
