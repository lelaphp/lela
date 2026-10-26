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
    vec![
        php_ext_date::extension(),
        php_ext_libxml::extension(),
        php_ext_lexbor::extension(),
        php_ext_random::extension(),
        php_ext_standard::extension(),
        php_ext_uri::extension(),
        php_ext_soap::extension(),
    ]
}

/// An engine of [`extensions`] and nothing else.
pub fn engine() -> Engine {
    Engine::builder().extensions(extensions()).build()
}
