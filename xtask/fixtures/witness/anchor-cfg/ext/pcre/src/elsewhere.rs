//! A file its own `#![cfg]` keeps out of every build but a `windows` one.
#![cfg(windows)]

use php_ext::attr::php_function;

#[php_function]
pub fn fixture_elsewhere() {}
