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
    &[
        #[cfg(feature = "standard")]
        php_ext_standard::extension,
        #[cfg(feature = "spl")]
        php_ext_spl::extension,
        #[cfg(feature = "date")]
        php_ext_date::extension,
        #[cfg(feature = "reflection")]
        php_ext_reflection::extension,
        #[cfg(feature = "pcre")]
        php_ext_pcre::extension,
        #[cfg(feature = "mbstring")]
        php_ext_mbstring::extension,
        #[cfg(feature = "session")]
        php_ext_session::extension,
        #[cfg(feature = "pdo")]
        php_ext_pdo::extension,
        #[cfg(feature = "dom")]
        php_ext_dom::extension,
        #[cfg(feature = "random")]
        php_ext_random::extension,
        #[cfg(feature = "pdo_mysql")]
        php_ext_pdo_mysql::extension,
        #[cfg(feature = "filter")]
        php_ext_filter::extension,
        #[cfg(feature = "tokenizer")]
        php_ext_tokenizer::extension,
        #[cfg(feature = "json")]
        php_ext_json::extension,
        #[cfg(feature = "openssl")]
        php_ext_openssl::extension,
        #[cfg(feature = "curl")]
        php_ext_curl::extension,
        #[cfg(feature = "iconv")]
        php_ext_iconv::extension,
        #[cfg(feature = "pdo_sqlite")]
        php_ext_pdo_sqlite::extension,
        #[cfg(feature = "ctype")]
        php_ext_ctype::extension,
        #[cfg(feature = "hash")]
        php_ext_hash::extension,
    ]
}

/// The tier, named once (design §4.3 seam 8): the interpreter alone until
/// the `jit` feature binds the compiled tier. Never in [`enabled()`].
pub type Tier = php_ext::Nop;
