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
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

pub mod css;
pub mod encoding;
pub mod html;
mod sys;
