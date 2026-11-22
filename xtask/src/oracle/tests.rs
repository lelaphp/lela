//! A pinned binary's identity is the `sha256` its table in the pin names
//! it by (ruled 2026-11-21: the md5 every pin carried first is no longer
//! fit to hold a download to, and the swap was one pin change of a
//! person's), read as a [`Sha256`] and nothing else, so a binary is held,
//! a kept run is keyed and a reading names a binary through one value.
//! These are the tests that hold the pin's tables to it.

use std::fs;
use std::path::Path;

use crate::manifest::{Crate, Layout};
use crate::oracle::{Pin, Platform, Runner, hosts};
use crate::scratch::Tree;
use crate::sha256::Sha256;

/// `sha256("php")`, as `shasum -a 256` prints it.
const PHP: &str = "12a5d18ee896e59954bdce0f4acc7212eebe03dae1834ef4ce160ac5afa5c4a8";

/// The grammar's least pin with php and php-cgi laid at `dir`, a variant's
/// Linux build and `[fastcgi]`, each table naming the digest of its
/// binary's bytes as `digest` spells it, each key followed by `, `.
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

fn sha256(bytes: &[u8]) -> String {
    format!("sha256 = \"{}\", ", Sha256::of(bytes))
}

/// An `md5` key, as every pin spelt one before the swap; its value is
/// never read.
fn md5(_: &[u8]) -> String {
    format!("md5 = \"{}\", ", "0".repeat(32))
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

/// A pin whose binaries carry `sha256` reads, every table held to its
/// SHA-256, and a run over a php of another SHA-256 is refused naming the
/// binary, the pin's digest and the binary's.
#[test]
fn a_pin_by_sha256_reads_and_a_binary_of_another_is_refused_by_name() {
    let tree = Tree::new("digest-sha256").unwrap();
    let pin = pin(tree.path(), sha256).unwrap();
    assert_eq!(pin.oracle.sha256, Sha256::of(b"php"));
    let build = pin.variant["v"].build(Platform::LinuxX86_64).unwrap();
    assert_eq!(build.sha256, Sha256::of(b"php"));
    pin.fastcgi().unwrap();
    let refused = refused_once_moved(tree.path(), &pin);
    for named in [format!("sha256:{PHP}"), Sha256::of(b"other").to_string()] {
        assert!(refused.contains(&named), "{refused}");
    }
}

/// A table of any of the pin's binaries that names an `md5`, alone or
/// beside its `sha256`, is refused naming the key as one the table does
/// not know: the md5 path is gone, and the pin is read as the swap left it.
#[test]
fn a_pin_entry_carrying_md5_is_refused_naming_the_key() {
    let tree = Tree::new("digest-md5-refused").unwrap();
    let both = |bytes: &[u8]| format!("{}{}", sha256(bytes), md5(bytes));
    let params = b"fastcgi_param  QUERY_STRING  $query_string;\n";
    let tables: [(&[u8], &str); 3] = [(b"php", "md5"), (b"cgi", "cgi = {"), (params, "md5")];
    for (bytes, named) in tables {
        for spelt in [&md5 as &dyn Fn(&[u8]) -> String, &both] {
            let digest = |of: &[u8]| if of == bytes { spelt(of) } else { sha256(of) };
            let refused = format!("{:#}", pin(tree.path(), digest).unwrap_err());
            assert!(refused.contains("unknown field `md5`"), "{refused}");
            assert!(refused.contains(named), "{refused}");
        }
    }
}

/// A pinned binary whose table names no `sha256` is refused, naming the
/// key it lacks.
#[test]
fn a_binary_pinned_by_no_sha256_is_refused_naming_the_key() {
    let tree = Tree::new("digest-none").unwrap();
    for bare in [&b"php"[..], b"cgi"] {
        let digest = |bytes: &[u8]| {
            if bytes == bare {
                String::new()
            } else {
                sha256(bytes)
            }
        };
        let refused = format!("{:#}", pin(tree.path(), digest).unwrap_err());
        assert!(refused.contains("missing field `sha256`"), "{refused}");
    }
}

/// The SHA-256 is the binary's identity: kept targets are keyed by it, and
/// a binary named by it is the control, and one of another is not.
#[test]
fn kept_targets_and_the_control_are_keyed_by_the_sha256() {
    let tree = Tree::new("digest-keys").unwrap();
    let mut pin = pin(tree.path(), sha256).unwrap();
    assert_eq!(
        crate::phpt::target::key(&pin),
        format!("pin-t-sha256-{PHP}")
    );
    let layout = Layout::of(&[Crate::named("sapi/cli")]);
    let php = tree.path().join("php");
    let named = Runner::resolve(tree.path(), &layout, &pin, Some(&php)).unwrap();
    assert_eq!(named, Runner::Control(php.clone()));
    pin.oracle.sha256 = Sha256::of(b"other");
    let named = Runner::resolve(tree.path(), &layout, &pin, Some(&php)).unwrap();
    assert_ne!(named, Runner::Control(php));
}
