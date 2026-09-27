//! A file only a `windows` build takes, by the `#[cfg]` on its `mod`.

use php_ext::attr::php_function;

#[php_function]
pub fn fixture_platform() {}
