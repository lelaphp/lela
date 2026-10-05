//! The engine's tests that belong to no one file of it: the Extension
//! seam's adapter, `TestExt`, the lifecycle it records (design §4.3),
//! the observer's order and pairing (seam 9) and the frames its
//! handlers read and keep, a periodic interrupt as
//! an extension sees it (#1806), and the pool's drain.

use crate::api;

mod frames;
mod lifecycle;
mod observer;
mod periodic;
mod pool;
mod stepped;
pub(crate) mod test_ext;
