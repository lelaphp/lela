//! The fixture's bundle: `enabled()` empty and the tier's alias, as
//! crates/php_bundle is laid down.
#![forbid(unsafe_code)]
use php_ext::abi::Extension;

/// The extensions in startup order, one per feature that is on.
pub fn enabled() -> &'static [fn() -> Extension] {
    &[]
}

/// The tier, named once; never in `enabled()`.
pub type Tier = php_ext::Nop;
