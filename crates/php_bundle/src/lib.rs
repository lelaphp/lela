//! The bundle (ADR-0004; design §4.1, §4.10, §7.1): the enabled set and
//! the tier's name, php-src's `internal_functions.c`. Data and nothing
//! else: `enabled()` is the extensions behind the crate's features in
//! startup order, a topological order of the `ext/*` graph that
//! `api-check` holds; `Tier` is the one alias the binary binds. The
//! engine links whatever it is handed; the tier is not in `enabled()`
//! and is absent from `get_loaded_extensions()`, as php's compiled tier
//! is.
#![forbid(unsafe_code)]

use php_ext::abi::Extension;

/// The extensions in startup order, one `extension()` per feature that
/// is on. `xtask new-ext <name>` appends the line behind its feature,
/// last; a person moves it when the crate declares an edge, and
/// `api-check` names both crates until it is moved.
#[must_use]
pub fn enabled() -> &'static [fn() -> Extension] {
    &[]
}

/// The tier, named once (design §4.3 seam 8): the interpreter alone until
/// the `jit` feature binds the compiled tier. Never in [`enabled()`].
pub type Tier = php_ext::Nop;
