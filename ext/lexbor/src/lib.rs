//! The `lexbor` extension: `php-src/ext/lexbor` as one crate under `ext/`
//! (ADR-0004), as php 8.5 has it, an extension of its own with no function
//! and no class, which `dom` requires. The library is php-src's vendored
//! lexbor, compiled by `build.rs` (design §7.2, #731) and bound in
//! [`ffi`] alone. One dependency, the facade; one entry, `extension()`; one
//! process start, lexbor's allocator pointed at the request's heap, and
//! no other state (ADR-0005, ADR-0010). Beside `extension()` the crate exports [`ffi`]'s safe
//! types to ext/dom, which includes lexbor's headers in php (design
//! §4.10). The phpt subset is `php-src/ext/lexbor/tests`, through `xtask
//! phpt --subset`.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

pub mod ffi;

use php_ext::abi::{Extension, ExtensionBuilder, Refusal};
use php_ext::raising::Level;
use php_ext::request::Request;

/// The extension value (design §7.1): `lexbor_module_entry`, an extension with
/// no function list, loaded before `dom`, which requires it, whose
/// `PHP_MINIT_FUNCTION(lexbor)` is its process start ([`Heap`]).
///
/// ```php
/// var_dump(extension_loaded('lexbor'));
/// $e = new ReflectionExtension('lexbor');
/// var_dump($e->getFunctions(), $e->getClasses(), $e->getVersion() === PHP_VERSION);
/// ```
/// ```expect
/// bool(true)
/// array(0) {
/// }
/// array(0) {
/// }
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .process_state(Heap::point)
        .info(lexbor_info)
        .build("lexbor")
}

/// The process start, `PHP_MINIT_FUNCTION(lexbor)`: lexbor's allocator
/// pointed at the request's heap ([`ffi::allocate_on_the_heap`]), once per
/// process.
struct Heap;

impl Heap {
    fn point() -> Result<Heap, Refusal> {
        if ffi::allocate_on_the_heap() {
            Ok(Heap)
        } else {
            Err(Refusal::new().raise(Level::Error, "lexbor_memory_setup()"))
        }
    }
}

/// `PHP_MINFO_FUNCTION(lexbor)`: `LEXBOR_VERSION_STRING` of the vendored
/// `lexbor/core/base.h`.
///
/// ```php
/// (new ReflectionExtension('lexbor'))->info();
/// ```
/// ```expect
///
/// lexbor
///
/// Lexbor support => active
/// Lexbor version => 2.7.0
/// ```
fn lexbor_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["Lexbor support", "active"], &["Lexbor version", "2.7.0"]]);
}
