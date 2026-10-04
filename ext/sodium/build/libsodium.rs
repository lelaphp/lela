//! What the linked libsodium has, read off the version `pkg-config
//! libsodium` reports: `build.rs`'s one decision, kept apart from it so
//! that `tests/build.rs` holds it to the releases it names.
//!
//! libsodium 1.0.19 added AEGIS-128L and AEGIS-256 (`crypto_aead_aegis128l_*`,
//! `crypto_aead_aegis256_*`), and php compiles its AEGIS functions and
//! constants only where `sodium.h` defines `crypto_aead_aegis128l_KEYBYTES`
//! and `crypto_aead_aegis256_KEYBYTES`, so a php built against 1.0.18, the
//! release Ubuntu ships, has none of them.

/// The first release with AEGIS-128L and AEGIS-256.
const AEGIS_ADDED: (u32, u32, u32) = (1, 0, 19);

/// Whether the libsodium of `version`, as `pkg-config --modversion`
/// prints it (`1.0.18`), has AEGIS-128L and AEGIS-256. A version this
/// cannot read is taken as the 1.0.18 floor `build.rs` holds the library
/// to, which has neither.
pub(crate) fn has_aead_aegis(version: &str) -> bool {
    release(version).is_some_and(|release| release >= AEGIS_ADDED)
}

/// The leading `major.minor.patch` of a version string.
fn release(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.trim().split('.');
    let mut number = || {
        let part = parts.next()?;
        let digits = part
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(part.len());
        part[..digits].parse().ok()
    };
    Some((number()?, number()?, number()?))
}
