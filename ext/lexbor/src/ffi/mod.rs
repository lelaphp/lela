//! The binding to the vendored lexbor (design §7.2, #731;
//! `unsafe.homes`' `ext/*/src/ffi/`): the one home of this crate's
//! `unsafe`, over what `build.rs` compiles. `php_lexbor.h` declares
//! nothing `PHPAPI`; php's dom includes lexbor's own headers, so what this
//! exports to ext/dom (design §4.10) is the lexbor calls dom's C makes,
//! each as a safe type:
//!
//! - [`html`]: the HTML5 parser of `html_document.c` and
//!   `html5_parser.c`, a document parsed chunk by chunk with its parse
//!   errors and observations, the fragment parse of
//!   `inner_outer_html_mixin.c`, and the tree walked through handles that
//!   borrow their document;
//! - [`encoding`]: the encodings, the streaming decoder and encoder and
//!   the `<meta>` pre-scan the same files drive;
//! - [`css`]: the selector parser of `parentnode/css_selectors.c`, its
//!   list copied into an owned tree for the matcher.
//!
//! and to ext/uri, whose `uri_parser_whatwg.c` includes `lexbor/url`:
//!
//! - [`url`]: the WHATWG URL parser, its setters and serialisers, and the
//!   validation errors it logs.
//!
//! Every lexbor call that can allocate, resize or free takes a [`Lent`],
//! the proof a request's heap is lent to it (ADR-0014), and only [`lent`]
//! makes one, from a request: a lexbor call outside a request is refused
//! by the types. The calls that touch no allocator take none: the
//! encoding tables, a decoder or an encoder over its own buffers, and the
//! reads of a parsed tree or URL. A value that holds lexbor's memory past
//! one call, the request's [`url::Parser`], is given back under a lend of
//! the request that made it.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

pub mod css;
pub mod encoding;
pub mod html;
mod sys;
pub mod url;

use php_ext::raising::Thrown;
use php_ext::request::Request;

/// The proof that a request's heap is lent to lexbor for the call at hand
/// (ADR-0014): what every lexbor entry that can allocate, resize or free
/// takes. Only [`lent`] makes one, and hands it by reference to a call
/// that cannot keep it, so none stands outside a lend.
pub struct Lent {
    _made_by_lent: (),
}

/// `call` served with `req`'s heap lent to lexbor and the proof handed
/// to it: what lexbor allocates meanwhile is the request's, counted as
/// php counts it, and what it frees of the request's goes back. The
/// `Err` is the memory limit's fatal, as php's `emalloc` bails out of
/// lexbor; what lexbor was handed in its place is the system's, and goes
/// back to it when its owner is given back under a later lend.
///
/// ext/uri makes every call of the request's URL parser through one, so
/// the log a parse leaves is the next call's to clean, as php's
/// `lxb_url_parser_clean` cleans it:
///
/// ```php
/// $e = []; var_dump(Uri\WhatWg\Url::parse("https://example.com:99999", null, $e), $e[0]->type);
/// $e = []; $u = Uri\WhatWg\Url::parse("https://example.com/", null, $e); var_dump($u->toAsciiString(), $e);
/// try { $u->withPort(99999); } catch (Uri\WhatWg\InvalidUrlException $x) { echo $x->getMessage(), "\n"; }
/// var_dump($u->withHost("xn--mnchen-3ya.de")->toUnicodeString());
/// ```
/// ```expect
/// NULL
/// enum(Uri\WhatWg\UrlValidationErrorType::PortOutOfRange)
/// string(20) "https://example.com/"
/// array(0) {
/// }
/// The specified port is malformed (PortOutOfRange)
/// string(20) "https://münchen.de/"
/// ```
///
/// # Errors
///
/// The limit's fatal, for the caller to end the request in.
pub fn lent<R>(req: &mut Request, call: impl FnOnce(&Lent) -> R) -> Result<R, Thrown> {
    req.lend_heap(|| call(&Lent { _made_by_lent: () }))
}
