//! The engine's tests that belong to no one file of it: the Extension
//! seam's adapter, `TestExt`, the lifecycle it records (design §4.3), and
//! a periodic interrupt as an extension sees it (#1806).

use crate::api;

mod lifecycle;
mod periodic;
mod test_ext;
