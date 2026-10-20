//! timelib on the request's heap (ADR-0014, design §5.14). php builds
//! timelib with its allocator pointed at `emalloc` (`config0.m4`'s
//! `timelib_config.h`), so every `timelib_time`, `timelib_tzinfo` and
//! error container it makes, and what it allocates while it parses, is
//! on php's request heap, where `memory_get_usage()` and its peak count
//! it. Here `timelib_sys` is compiled against the same header and
//! `php_heap`'s `emalloc`, which reach the request's heap while the
//! request lends it ([`Memory::lend_heap`]).
//!
//! [`Lent`] is the proof a lend is in force: every call into timelib that
//! can allocate or free takes one, and only [`lent`] makes one, so no
//! such call runs without its request's heap. A value that holds
//! timelib's memory ([`crate::ffi::Time`], a zone, an error container)
//! is freed by its drop, and a drop under a lend gives the memory back;
//! outside one `efree` frees nothing and the block is the request's
//! end's. So each is dropped under a lend where php frees it: [`release`].

use php_ext::raising::Thrown;
use php_ext::request::Request;

use crate::State;

/// The request's heap lent to timelib, and the date state beside it,
/// what the parsers' zone wrapper reads and `DATEG(last_errors)` keeps.
pub(crate) struct Lent<'a> {
    state: &'a mut State,
}

impl Lent<'_> {
    /// The date state, `DATEG`.
    pub(crate) fn state(&mut self) -> &mut State {
        self.state
    }

    /// A lend for a test with no request, whose timelib calls are the
    /// system allocator's.
    #[cfg(test)]
    pub(crate) fn detached(state: &mut State) -> Lent<'_> {
        Lent { state }
    }
}

/// `call` served with the request's heap lent to timelib and the date
/// state handed beside it. The `Err` is the memory limit's fatal.
pub(crate) fn lent<R>(
    req: &mut Request,
    call: impl FnOnce(&mut Lent<'_>) -> R,
) -> Result<R, Thrown> {
    let mut state = std::mem::take(req.state::<State>());
    let answered = req
        .memory_mut()
        .lend_heap(|| call(&mut Lent { state: &mut state }));
    *req.state::<State>() = state;
    answered
}

/// `value` dropped under a lend, so what it holds of timelib's goes back
/// to the request's heap: php's `timelib_time_dtor`,
/// `timelib_tzinfo_dtor` and `timelib_error_container_dtor` where php
/// calls them. A drop reads nothing of the date state, so the state
/// stays where it is.
pub(crate) fn release<T>(req: &mut Request, value: T) -> Result<(), Thrown> {
    req.memory_mut().lend_heap(|| drop(value))
}

/// `call` served with `value` standing, then `value` released whatever
/// `call` answered: a timelib struct php makes at the top of a function
/// and frees with `timelib_*_dtor` at its end, around work that needs the
/// request.
pub(crate) fn kept<T, R>(
    req: &mut Request,
    value: T,
    call: impl FnOnce(&mut Request, &T) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let answered = call(req, &value);
    release(req, value)?;
    answered
}
