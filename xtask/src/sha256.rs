//! A SHA-256 digest as the harness's own files spell one, `sha256:<hex>`:
//! the `[closure]` a variant's macOS build was made against (ruled
//! 2026-10-18) and the two sides' lines an accepted `trace:dump/` row was
//! ruled on (harness revision 168). [`Sha256::of`] is the one place bytes
//! become one, and a file's spelling is decoded here and nowhere else, so
//! a digest in hand is 32 bytes and never a string that might not be one.

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// What a digest is spelt after.
const PREFIX: &str = "sha256:";

/// A SHA-256 digest, read and written as `sha256:<64 lowercase hex>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Sha256([u8; 32]);

impl Sha256 {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Sha256 {
        use sha2::Digest as _;
        Sha256(sha2::Sha256::digest(bytes).into())
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl FromStr for Sha256 {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Sha256> {
        let spelt = || format!("{text:?} is not a digest spelt {PREFIX}<64 lowercase hex digits>");
        let hex = text.strip_prefix(PREFIX).with_context(spelt)?;
        let lower = |b: &u8| b.is_ascii_digit() || (b'a'..=b'f').contains(b);
        ensure!(hex.len() == 64 && hex.bytes().all(|b| lower(&b)), spelt());
        let mut digest = [0; 32];
        for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
        }
        Ok(Sha256(digest))
    }
}

impl From<Sha256> for String {
    fn from(digest: Sha256) -> String {
        digest.to_string()
    }
}

impl TryFrom<String> for Sha256 {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Sha256> {
        text.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::Sha256;

    /// A digest is spelt as `sha256sum` prints it, after the prefix, and
    /// reads back as itself; a spelling of another length, case or prefix
    /// is refused.
    #[test]
    fn a_digest_reads_back_as_it_is_spelt_and_no_other_spelling_reads() {
        let empty = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(Sha256::of(b"").to_string(), empty);
        assert_eq!(empty.parse::<Sha256>().unwrap(), Sha256::of(b""));
        let upper = empty.to_uppercase();
        for refused in [&empty[7..], &empty[..70], upper.as_str(), "md5:00"] {
            assert!(refused.parse::<Sha256>().is_err(), "{refused}");
        }
    }
}
