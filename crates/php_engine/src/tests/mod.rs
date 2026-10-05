//! The engine's tests that belong to no one file of it: the Extension
//! seam's adapter, `TestExt`, the lifecycle it records (design §4.3),
//! the observer's order and pairing (seam 9), its notices and the
//! handlers added while a request is served, a periodic interrupt as
//! an extension sees it (#1806), and the pool's drain.

use crate::api;

mod lifecycle;
mod observer;
mod observer_notices;
mod periodic;
mod pool;
pub(crate) mod test_ext;
