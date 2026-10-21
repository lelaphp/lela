//! What every test binary of this extension builds its engine from: the
//! extensions it requires and itself.

#![allow(
    dead_code,
    reason = "each test binary uses its own part of what is shared"
)]

use php_ext::abi::Extension;
use php_ext::test::Engine;

/// The extensions this one requires, then itself.
pub fn extensions() -> Vec<Extension> {
    vec![php_ext_pcre::extension(), php_ext_mbstring::extension()]
}

/// An engine of [`extensions`] and nothing else.
pub fn engine() -> Engine {
    Engine::builder().extensions(extensions()).build()
}
