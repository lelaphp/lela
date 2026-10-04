//! What the linked libxml2 exports, read off the version `pkg-config
//! libxml-2.0` reports: `build.rs`'s one decision, kept apart from it so
//! that `tests/build.rs` holds it to the releases it names.
//!
//! libxml2 2.14.0 renamed `valuePush` and `valuePop` to
//! `xmlXPathValuePush` and `xmlXPathValuePop` (upstream 92d7b0cd), with
//! the signatures unchanged, and stopped exporting the old names; its
//! `xpathInternals.h` keeps them as macros over the new ones, which is how
//! php's `valuePush(ctxt, ...)` in `ext/dom/xpath_callbacks.c` links
//! against either. A Rust declaration names the symbol itself, so
//! `build.rs` picks the one the library exports.

/// The first release whose library exports `xmlXPathValuePush` and
/// `xmlXPathValuePop` and no longer `valuePush` and `valuePop`.
const XPATH_VALUE_RENAMED: (u32, u32) = (2, 14);

/// Whether the libxml2 of `version`, as `pkg-config --modversion` prints
/// it (`2.15.2`), exports `xmlXPathValuePush` and `xmlXPathValuePop`. A
/// version this cannot read is taken as the 2.9 the declarations are
/// written to, the floor `build.rs` holds the library to.
pub(crate) fn exports_xpath_value_push(version: &str) -> bool {
    major_minor(version).is_some_and(|release| release >= XPATH_VALUE_RENAMED)
}

/// The leading `major.minor` of a version string.
fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim().split('.');
    let mut number = || {
        let part = parts.next()?;
        let digits = part
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(part.len());
        part[..digits].parse().ok()
    };
    Some((number()?, number()?))
}
