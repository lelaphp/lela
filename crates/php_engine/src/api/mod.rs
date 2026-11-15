//! The native face (design §4.4): the one interface an extension sees,
//! re-exported whole by `php_ext` and measured there as `api.lock`. Nine
//! groups, eight mirroring the census: values, raising, coercion, calling
//! back, the request environment, declarations, the ABI, and the token
//! stream the tokenizer reads; the ninth the process's identity, which the
//! engine owns (revision 41); and `__private`, what only a macro
//! expansion names, held to a cap of its own. `php_streams` adds its
//! streams facet to the request group in `php_ext`, one face. What the
//! stream layer alone reaches, the socket and fd calls among it, is
//! [`crate::layer`], counted in `layer.lock`. Every item is a
//! line of `api.lock`; a new `pub` here lands with its line.

#[doc(hidden)]
pub mod __private;
pub mod abi;
pub mod call;
pub mod coerce;
pub mod decl;
pub mod process;
pub mod raising;
pub mod request;
pub mod syntax;
pub mod values;
