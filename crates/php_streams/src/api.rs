//! The streams facet of the native face (design §4.3 seam 5, §4.4): the
//! items [`request`] adds to the engine's request group, which `php_ext`
//! re-exports beside the engine's own. The engine's groups are named here
//! as the layer's code names them, `crate::api::raising` and its kin.

pub mod request;

pub(crate) use php_engine::api::{abi, call, coerce, decl, raising, values};
pub(crate) use php_engine::layer;
