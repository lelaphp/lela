//! The native face (design §4.4): the one interface an extension sees,
//! re-exported whole by `php_ext` and measured there as `api.lock`. Seven
//! groups, mirroring the census: values, raising, coercion, calling back,
//! the request environment, declarations, and the ABI. Every item is a
//! line of `api.lock`; a new `pub` here lands with its line.

pub mod abi;
pub mod call;
pub mod coerce;
pub mod decl;
pub mod raising;
pub mod request;
pub mod values;
