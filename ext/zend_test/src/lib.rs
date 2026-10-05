//! The `zend_test` extension: `php-src/ext/zend_test` as one crate under
//! `ext/` (ADR-0004), off by default as php builds it only with
//! `--enable-zend-test`, and held by the oracle variant that loads it
//! (`oracle = "zend-test"`, #1782). Beside the facade it depends on
//! ext/standard alone, for `php_var_export_ex`; one entry,
//! `extension()`; its state on the value (ADR-0005). The phpt subset is
//! every `.phpt` under `php-src/ext/zend_test`, through `xtask phpt
//! --subset`.
//!
//! What is ported is the part #1782 ruled: the observer, `observer.c`
//! ([`observer`]), the one bundled user of design §4.3 seam 9, whose
//! tests set its ini switches. The rest of php's test extension, its
//! test functions and classes, comes only with a raise of the crate's
//! ceiling. Two more parts land here as their engine sides are built:
//! the observed opcodes, `zend_test.observer.show_opcode` and
//! `show_opcode_in_user_handler` (#1824), whose entries are declared
//! already and whose tags [`observer`] leaves a place for, and the
//! allocator hooks of `zend_mm_custom_handlers.c` (#1825), a file of
//! their own beside [`observer`].
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

mod observer;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::request::Request;

/// The extension value (design §7.1): `zend_test_module_entry` as far
/// as this crate carries it, the observer ([`observer::declared`]) and the
/// `MINFO` function.
///
/// ```php
/// var_dump(extension_loaded('zend_test'), ini_get('zend_test.observer.enabled'), ini_get('zend_test.observer.show_output'));
/// var_dump(ini_set('zend_test.observer.observe_function_names', 'foo'), ini_get('zend_test.observer.observe_function_names'));
/// ```
/// ```expect
/// bool(true)
/// string(1) "0"
/// string(1) "1"
/// bool(false)
/// string(0) ""
/// ```
pub fn extension() -> Extension {
    observer::declared(ExtensionBuilder::default())
        .info(info)
        .build("zend_test")
}

/// `PHP_MINFO_FUNCTION(zend_test)`: the one row, then the ini entries.
fn info(req: &mut Request) {
    req.info_table(&[&["zend_test extension", "enabled"]]);
    let names: Vec<&[u8]> = observer::ENTRIES.iter().map(|entry| entry.0).collect();
    req.info_ini_entries(&names);
}
