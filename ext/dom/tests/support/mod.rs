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
        php_ext_libxml::extension(),
        php_ext_lexbor::extension(),
        php_ext_standard::extension(),
        php_ext_uri::extension(),
        php_ext_dom::extension(),
    ]
}

/// An engine of [`extensions`] and nothing else.
pub fn engine() -> Engine {
    Engine::builder().extensions(extensions()).build()
}

/// An engine of [`extensions`] with spl and the json it requires, for a
/// test that registers an autoloader.
pub fn engine_with_spl() -> Engine {
    let mut extensions = extensions();
    let at = extensions.len() - 1;
    extensions.splice(
        at..at,
        [php_ext_json::extension(), php_ext_spl::extension()],
    );
    Engine::builder().extensions(extensions).build()
}
