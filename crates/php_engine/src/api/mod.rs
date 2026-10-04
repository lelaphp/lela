//! The native face (design §4.4): the one interface an extension sees,
//! re-exported whole by `php_ext` and measured there as `api.lock`. Ten
//! groups, eight mirroring the census: values, raising, coercion, calling
//! back, the request environment, declarations, the ABI, and the token
//! stream the tokenizer reads; the ninth the process's identity, which the
//! engine owns (revision 41); the tenth the socket and fd calls, which the
//! stream layer above the engine makes (design §4.3 seam 5); and
//! `__private`, what only a macro expansion names, held to a cap of its
//! own. `php_streams` adds its streams facet to the request group in
//! `php_ext`, one face. Every item is a line of `api.lock`; a new `pub`
//! here lands with its line.

#[doc(hidden)]
pub mod __private;
pub mod abi;
pub mod call;
pub mod coerce;
pub mod decl;
pub mod net;
pub mod process;
pub mod raising;
pub mod request;
pub mod syntax;
pub mod values;
