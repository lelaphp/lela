//! A pinned binary's identity: the digest its table in the pin names it
//! by, the `md5` every pin carried first or the `sha256` that replaces it
//! (ruled 2026-11-21: the md5 is no longer fit to hold a download to),
//! decoded here once, so a binary is held, a kept run is keyed and a
//! reading names a binary through one value and never through the key it
//! was spelt under.
//!
//! The swap is one pin change of a person's, so the reader takes either
//! key, or both while a pin carries the two: the SHA-256 is then the one
//! held, and the md5 beside it is read only so `status --section oracle`
//! still prints it to the CI scripts that read it.

use std::fmt;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::sha256::Sha256;

/// The keys a pinned binary's table names its digest under, as the file
/// spells them.
#[derive(Deserialize)]
struct Keys {
    md5: Option<String>,
    sha256: Option<Sha256>,
}

/// The digest a pin holds one binary to: its md5 where it pins no
/// SHA-256, and its SHA-256 wherever it pins one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Keys")]
pub enum Digest {
    /// `md5` alone, held as every pin was before the swap.
    Md5(String),
    /// `sha256`, and the `md5` beside it where the pin still carries one,
    /// which nothing holds.
    Sha256 { sha256: Sha256, md5: Option<String> },
}

impl Digest {
    /// Whether `bytes` are the binary this digest names: `Err` carries
    /// their digest as this one is taken, for the refusal to name.
    pub fn holds(&self, bytes: &[u8]) -> Result<(), Digest> {
        match self {
            Digest::Md5(pinned) => {
                let actual = format!("{:x}", md5::compute(bytes));
                (actual == *pinned).then_some(()).ok_or(Digest::Md5(actual))
            }
            Digest::Sha256 { sha256, .. } => {
                let actual = Sha256::of(bytes);
                (actual == *sha256).then_some(()).ok_or(Digest::Sha256 {
                    sha256: actual,
                    md5: None,
                })
            }
        }
    }

    /// The md5 the pin carries, held or beside a SHA-256.
    pub fn md5(&self) -> Option<&str> {
        match self {
            Digest::Md5(md5) | Digest::Sha256 { md5: Some(md5), .. } => Some(md5),
            Digest::Sha256 { md5: None, .. } => None,
        }
    }

    /// The SHA-256 the pin carries, none where it pins the md5 alone.
    pub fn sha256(&self) -> Option<Sha256> {
        match self {
            Digest::Md5(_) => None,
            Digest::Sha256 { sha256, .. } => Some(*sha256),
        }
    }

    /// The digest held, as a file's name spells it: the md5's hex as kept
    /// names always were, and `sha256-<hex>` for a SHA-256.
    pub fn key(&self) -> String {
        match self {
            Digest::Md5(md5) => md5.clone(),
            Digest::Sha256 { sha256, .. } => sha256.to_string().replacen(':', "-", 1),
        }
    }
}

/// The digest of a binary no pin names: [`super::Pin`]'s default, and a
/// variant's binary on a machine its pin names no build for, which
/// `unpinned` refuses before any digest is taken.
impl Default for Digest {
    fn default() -> Digest {
        Digest::Md5(String::new())
    }
}

/// The digest held, as a message names a binary by it: `md5 <hex>` or
/// `sha256:<hex>`.
impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Digest::Md5(md5) => write!(f, "md5 {md5}"),
            Digest::Sha256 { sha256, .. } => write!(f, "{sha256}"),
        }
    }
}

impl TryFrom<Keys> for Digest {
    type Error = anyhow::Error;

    fn try_from(keys: Keys) -> Result<Digest> {
        Ok(match keys {
            Keys {
                sha256: Some(sha256),
                md5,
            } => Digest::Sha256 { sha256, md5 },
            Keys {
                md5: Some(md5),
                sha256: None,
            } => Digest::Md5(md5),
            Keys {
                md5: None,
                sha256: None,
            } => bail!(
                "a pinned binary names neither `md5` nor `sha256`, and is held to one of them"
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::Digest;
    use crate::manifest::{Crate, Layout};
    use crate::oracle::{Pin, Platform, Runner, hosts};
    use crate::scratch::Tree;
    use crate::sha256::Sha256;

    /// `sha256("php")`, as `shasum -a 256` prints it.
    const PHP: &str = "12a5d18ee896e59954bdce0f4acc7212eebe03dae1834ef4ce160ac5afa5c4a8";

    /// The grammar's least pin with php and php-cgi laid at `dir`, a
    /// variant's Linux build and `[fastcgi]`, each table naming the digest
    /// of its binary's bytes as `digest` spells it, each key followed by
    /// `, `.
    fn pin(dir: &Path, digest: impl Fn(&[u8]) -> String) -> anyhow::Result<Pin> {
        let (php, cgi) = (dir.join("php"), dir.join("php-cgi"));
        fs::write(&php, "php").unwrap();
        fs::write(&cgi, "cgi").unwrap();
        let params = "fastcgi_param  QUERY_STRING  $query_string;\n";
        let (of_php, of_cgi) = (digest(b"php"), digest(b"cgi"));
        let lines = |keys: String| keys.replace(", ", "\n");
        Pin::parse(&format!(
            "[oracle]\nversion = \"9.1.1\"\npath = {php:?}\n{}\
             [source]\ntag = \"t\"\ncommit = \"c\"\nphpt_files = 0\n\
             [recipe]\nscript_name = \"w.php\"\npreamble = \"\"\nargs = []\nini = []\nenv_clear = true\n\
             [recipe.limits]\ntimeout_s = 1\n\
             [recipe.normalise]\ncrlf_to_lf = true\ntrim = \"\"\nscratch_token = \"%S%\"\n\
             [variant.v]\nflags = []\nlinux-x86_64 = {{ path = \"/opt/php\", {of_php}image = \"i\" }}\n\
             [sapi]\ncgi = {{ {of_cgi}path = {cgi:?} }}\n\
             [fastcgi]\nnginx = \"1\"\nsource = \"s\"\n{}\
             script_filename = \"$document_root$fastcgi_script_name\"\nparams = {params:?}\n",
            lines(digest(b"php")),
            lines(digest(params.as_bytes())),
        ))
    }

    fn md5(bytes: &[u8]) -> String {
        format!("md5 = \"{:x}\", ", md5::compute(bytes))
    }

    fn sha256(bytes: &[u8]) -> String {
        format!("sha256 = \"{}\", ", Sha256::of(bytes))
    }

    /// The run over the pin's php refused once php's bytes are another's.
    fn refused_once_moved(dir: &Path, pin: &Pin) -> String {
        pin.oracle().asserted().unwrap();
        let cgi = pin.sapi.named(hosts::CGI).unwrap();
        pin.oracle().host_asserted(cgi).unwrap();
        fs::write(dir.join("php"), "other").unwrap();
        let refused = pin.oracle().asserted().unwrap_err().to_string();
        assert!(refused.contains(&dir.join("php").display().to_string()));
        refused
    }

    /// A pin whose binaries carry `sha256` and no `md5` reads, every table
    /// held to its SHA-256, and a run over a php of another SHA-256 is
    /// refused naming the binary, the pin's digest and the binary's.
    #[test]
    fn a_pin_by_sha256_alone_reads_and_a_binary_of_another_is_refused_by_name() {
        let tree = Tree::new("digest-sha256").unwrap();
        let pin = pin(tree.path(), sha256).unwrap();
        let of = |bytes: &[u8]| Digest::Sha256 {
            sha256: Sha256::of(bytes),
            md5: None,
        };
        assert_eq!(pin.oracle.digest, of(b"php"));
        let build = pin.variant["v"].build(Platform::LinuxX86_64).unwrap();
        assert_eq!(build.digest, of(b"php"));
        pin.fastcgi().unwrap();
        let refused = refused_once_moved(tree.path(), &pin);
        for named in [format!("sha256:{PHP}"), Sha256::of(b"other").to_string()] {
            assert!(refused.contains(&named), "{refused}");
        }
    }

    /// A pin by `md5` alone reads and runs as every pin did before.
    #[test]
    fn a_pin_by_md5_alone_reads_and_runs_as_before() {
        let tree = Tree::new("digest-md5").unwrap();
        let pin = pin(tree.path(), md5).unwrap();
        let of = |bytes: &[u8]| Digest::Md5(format!("{:x}", md5::compute(bytes)));
        assert_eq!(pin.oracle.digest, of(b"php"));
        let refused = refused_once_moved(tree.path(), &pin);
        assert!(refused.contains(&of(b"php").to_string()), "{refused}");
        assert!(refused.contains(&of(b"other").to_string()), "{refused}");
    }

    /// A pinned binary whose table names neither digest is refused, the
    /// refusal naming the table it stands in.
    #[test]
    fn a_binary_pinned_by_neither_digest_is_refused_by_name() {
        let tree = Tree::new("digest-neither").unwrap();
        for (bare, named) in [(&b"php"[..], "[oracle]"), (b"cgi", "cgi")] {
            let digest = |bytes: &[u8]| {
                if bytes == bare {
                    String::new()
                } else {
                    md5(bytes)
                }
            };
            let refused = format!("{:#}", pin(tree.path(), digest).unwrap_err());
            assert!(refused.contains("neither `md5` nor `sha256`"), "{refused}");
            assert!(refused.contains(named), "{refused}");
        }
    }

    /// Where the pin carries a SHA-256, it is the binary's identity: kept
    /// targets are keyed by it, and a binary named by it is the control,
    /// whatever md5 stands beside it.
    #[test]
    fn kept_targets_and_the_control_are_keyed_by_the_sha256_where_pinned() {
        let tree = Tree::new("digest-keys").unwrap();
        let beside = |bytes: &[u8]| format!("{}{}", sha256(bytes), md5(b"other"));
        let mut pin = pin(tree.path(), beside).unwrap();
        assert_eq!(
            crate::phpt::target::key(&pin),
            format!("pin-t-sha256-{PHP}")
        );
        let layout = Layout::of(&[Crate::named("sapi/cli")]);
        let php = tree.path().join("php");
        let named = Runner::resolve(tree.path(), &layout, &pin, Some(&php)).unwrap();
        assert_eq!(named, Runner::Control(php.clone()));
        pin.oracle.digest = Digest::Md5(format!("{:x}", md5::compute(b"other")));
        let named = Runner::resolve(tree.path(), &layout, &pin, Some(&php)).unwrap();
        assert_ne!(named, Runner::Control(php));
    }
}
