//! The engine's tests that belong to no one file of it: the Extension
//! seam's adapter, `TestExt`, the lifecycle it records (design §4.3),
//! the observer's order and pairing (seam 9), the frames its handlers
//! read and keep, its notices and the handlers added while a request is
//! served, a periodic interrupt as an extension sees it (#1806), the
//! pool's drain, a handler that blocks with the time limit suspended
//! (#1814), allocations as an extension observes them (design §7.1),
//! and the statements a Zend extension is told of under extended info
//! (#1823).

use crate::api;

mod allocations;
mod frames;
mod lifecycle;
mod observer;
mod observer_notices;
mod periodic;
mod pool;
mod statements;
mod stepped;
pub(crate) mod test_ext;
mod time_limit;
