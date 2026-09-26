//! The native face (design §4.4): the one interface an extension sees,
//! re-exported whole by `php_ext` and measured there as `api.lock`. Eight
//! groups, mirroring the census: values, raising, coercion, calling back,
//! the request environment, declarations, the ABI, and the token stream
//! the tokenizer reads; and `__private`, what only a macro expansion
//! names, held to a cap of its own. Every item is a line of `api.lock`; a
//! new `pub` here lands with its line.

#[doc(hidden)]
pub mod __private;
pub mod abi;
pub mod call;
pub mod coerce;
pub mod decl;
pub mod raising;
pub mod request;
pub mod syntax;
pub mod values;
