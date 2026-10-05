//! The engine's tests that belong to no one file of it: the Extension
//! seam's adapter, `TestExt`, the lifecycle it records (design §4.3),
//! the observer's order and pairing (seam 9), the frames its handlers
//! read, keep and write and the op arrays they read (#1828, #1829), a
//! periodic interrupt as an extension sees it (#1806), and the pool's drain.

use crate::api;

mod frame_writes;
mod frames;
mod lifecycle;
mod observer;
mod op_arrays;
mod periodic;
mod pool;
mod stepped;
pub(crate) mod test_ext;
