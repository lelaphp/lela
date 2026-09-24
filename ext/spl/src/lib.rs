//! The `spl` extension: `php-src/ext/spl` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/spl/tests`, through `xtask phpt --subset`.
//!
//! Each class is declared off the text of its `*.stub.php` ([`stub`])
//! in the order `PHP_MINIT_FUNCTION(spl)` registers it, and where php
//! gives the class an object of its own (`spl_array_object`,
//! `spl_fixedarray_object`) the object carries it as the native
//! payload its class's `create_object` lays down (design §5.4), with
//! the `ObjectHandlers` php overrides beside it. What a payload holds
//! of the heap is given back by the table's `free_obj` and named to the
//! collector by its `get_gc`. The natives of `php_spl.c` are
//! [`functions`], over the engine's autoload stack.
#![forbid(unsafe_code)]
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

mod exceptions;
mod iterators;
mod observer;
mod stub;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

/// A native's entry, the shape `ExtensionBuilder::functions` pairs a
/// declaration with.
pub(crate) type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it, the classes in the order
/// `PHP_MINIT_FUNCTION(spl)` registers them.
///
/// ```php
/// var_dump(extension_loaded('spl'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .classes(exceptions::classes())
        .classes(iterators::classes())
        .classes(observer::classes())
        .request_state::<State>()
        .info(spl_info)
        .build("SPL")
}

/// `PHP_MINFO_FUNCTION(spl)`: `SPL support => enabled`, and the two
/// lists of interfaces and classes the full table carries once its
/// classes are declared.
fn spl_info(req: &mut Request) {
    req.info_table(&[&["SPL support", "enabled"]]);
}
