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
//!   list copied into an owned tree for the matcher;
//! - [`array`]: lexbor's array of structs, the stack `html5_parser.c`
//!   keeps its bridge's work list in.
//!
//! and to ext/uri, whose `uri_parser_whatwg.c` includes `lexbor/url`:
//!
//! - [`url`]: the WHATWG URL parser, its setters and serialisers, and the
//!   validation errors it logs.
//!
//! lexbor allocates through the request's heap, as php's
//! `PHP_MINIT_FUNCTION(lexbor)` points it at `emalloc`, `erealloc`,
//! `ecalloc` and `efree` (`Z/ext/lexbor/php_lexbor.c:63`):
//! [`allocate_on_the_heap`] points it at `php_heap`'s shims of the four
//! (ADR-0014) at the extension's process start, so what an HTML5 parse,
//! a selector list or a URL holds counts in `memory_get_usage()`, its
//! peak and `memory_limit`.
//!
//! Every lexbor call that can allocate, resize or free takes a [`Lent`],
//! the proof a request's heap is lent to it (ADR-0014), and only [`lent`]
//! makes one, from a request: a lexbor call outside a request is refused
//! by the types, so none allocates outside a lend. The calls that touch
//! no allocator take none: the encoding tables, a decoder or an encoder
//! over its own buffers, and the reads of a parsed tree or URL. A value
//! that holds lexbor's memory past one call has no drop: it is an
//! [`Owned`], given back under a lend of the request that made it,
//! [`kept`] across the work that needs the request between its calls,
//! or, the request's [`url::Parser`], destroyed at its `post_deactivate`.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

pub mod array;
pub mod css;
pub mod encoding;
pub mod html;
mod sys;
pub mod url;

use php_ext::raising::Thrown;
use php_ext::request::{Request, ecalloc, efree, emalloc, erealloc};

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
    req.memory_mut()
        .lend_heap(|| call(&Lent { _made_by_lent: () }))
}

/// A value that holds lexbor's memory past the call that made it, with no
/// drop of its own: outside a lend `efree` frees nothing (ADR-0014), so
/// it is given back under a lend of the request that made it.
pub trait Owned {
    /// What the value holds of lexbor's given back to the request's heap.
    fn give_back(self, lent: &Lent);
}

/// `value` kept across `call`, which reaches the request between the
/// lexbor calls it makes on it, then given back under a lend whatever
/// `call` answered: php's `lxb_html_document_create` at the top of a
/// function and its `lxb_html_document_destroy` on every path out. A
/// parse an error handler throws out of gives its document back, and the
/// request's memory reads as it did before it:
///
/// ```php
/// set_error_handler(function () { throw new Exception('handler'); });
/// for ($i = 0; $i < 2; $i++) {
///     $m = memory_get_usage();
///     try { Dom\HTMLDocument::createFromString('<p>x</b>'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
///     unset($e);
/// }
/// var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// handler
/// handler
/// int(0)
/// ```
///
/// # Errors
///
/// What `call` answered, or the memory limit's fatal.
pub fn kept<T: Owned, R>(
    req: &mut Request,
    mut value: T,
    call: impl FnOnce(&mut Request, &mut T) -> Result<R, Thrown>,
) -> Result<R, Thrown> {
    let answered = call(req, &mut value);
    let given = lent(req, |lent| value.give_back(lent));
    let answered = answered?;
    given.map(|()| answered)
}

/// php's `PHP_MINIT_FUNCTION(lexbor)`: `lexbor_memory_setup` with the
/// heap's `emalloc`, `erealloc`, `ecalloc` and `efree` (ADR-0014), once
/// per process, before any engine carrying the extension serves a
/// request. lexbor's allocator is the process's, so every caller moves
/// at once, and every caller's call is under a lend ([`Lent`]); one made
/// outside a request would allocate as the shims do there, from the
/// system, and free nothing, which the types refuse. `false` where lexbor
/// refused the four. So an HTML5 parse grows the request's peak by what
/// lexbor holds while it works:
///
/// ```php
/// $html = str_repeat('<div class=a><p>hello <b>world</b></p></div>', 2000);
/// $m = memory_get_usage(); memory_reset_peak_usage();
/// $d = Dom\HTMLDocument::createFromString($html, LIBXML_NOERROR);
/// var_dump(memory_get_peak_usage() - $m > 2000000);
/// ```
/// ```expect
/// bool(true)
/// ```
pub(crate) fn allocate_on_the_heap() -> bool {
    // SAFETY: four functions of the signatures lexbor's typedefs name,
    // written into its allocator before any request of an engine carrying
    // the extension reaches lexbor, which only a lend does.
    let status = unsafe { sys::lexbor_memory_setup(emalloc, erealloc, ecalloc, efree) };
    status == sys::LXB_STATUS_OK
}
