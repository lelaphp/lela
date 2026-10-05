//! The observer word (design §4.3 seam 9), as G1 measures it (#1808):
//! one engine-level word, fixed at engine build, read at the call and
//! return homes, and nothing past it while no observer is declared. php's
//! `ZEND_OBSERVER_FCALL_BEGIN` and `ZEND_OBSERVER_FCALL_END`
//! (`Z/Zend/zend_observer.h:49-58`) are the same test of
//! `zend_observer_fcall_op_array_extension` against `-1`; php leaves it
//! out of the plain handlers and keeps it in their observer-specialised
//! variants, which is the escape the design names if this test costs.
//! The seam behind the word, the per-function slot resolved on a first
//! call and the begin and end handlers, is #1809's.

use crate::api::request::Request;

/// The call home: a user frame activated, its prologue having raised
/// nothing, or a native about to be served.
#[inline(always)] // design §4.3 seam 9: the home pays the load and the branch, never a call
#[allow(clippy::inline_always)]
pub(crate) fn call(req: &mut Request) {
    if req.shared.observers != 0 {
        begin(req);
    }
}

/// The return home: a user frame about to be popped by its `RETURN`, or
/// a native that has been served.
#[inline(always)] // as `call`
#[allow(clippy::inline_always)]
pub(crate) fn returned(req: &mut Request) {
    if req.shared.observers != 0 {
        end(req);
    }
}

/// Where the begin handlers will be served. Out of line and cold, so the call
/// home pays the test and the branch alone; opaque to the optimiser, so
/// the branch is not folded away as one over an empty body.
#[cold]
#[inline(never)]
fn begin(req: &mut Request) {
    std::hint::black_box(req);
}

/// Where the end handlers will be served, as [`begin`] is.
#[cold]
#[inline(never)]
fn end(req: &mut Request) {
    std::hint::black_box(req);
}
