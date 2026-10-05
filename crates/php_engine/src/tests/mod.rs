//! The engine's tests that belong to no one file of it: the Extension
//! seam's adapter, `TestExt`, the lifecycle it records (design §4.3),
//! the observer's order and pairing (seam 9), a periodic interrupt as
//! an extension sees it (#1806), and the pool's drain.

use crate::api;

mod lifecycle;
mod observer;
mod periodic;
mod pool;
mod stepped;
pub(crate) mod test_ext;
