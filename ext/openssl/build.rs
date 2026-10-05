//! The two constants php registers from `<openssl/opensslv.h>`,
//! `OPENSSL_VERSION_TEXT` and `OPENSSL_VERSION_NUMBER` (`@cvalue` in
//! `Z/ext/openssl/openssl.stub.php`), are the values of the headers a build
//! compiles against, not the libcrypto it later loads (#1958, #1974). They
//! are read here, from the headers openssl-sys found: the number is the one
//! openssl-sys hands a dependent as `DEP_OPENSSL_VERSION_NUMBER`, which it
//! synthesizes from the header's `OPENSSL_VERSION_MAJOR`, `_MINOR` and
//! `_PATCH` as the header's own macro does; the text is the header's macro
//! as the C preprocessor expands it under `DEP_OPENSSL_INCLUDE`. Both are
//! written to `$OUT_DIR/header.rs` as Rust constants, and a header that
//! cannot be read fails the build.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The line the preprocessor expands: the marker, then the macro.
const SOURCE: &str = "#include <openssl/opensslv.h>\nphp_openssl_header OPENSSL_VERSION_TEXT\n";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if let Err(error) = run() {
        println!("cargo::error={error}");
    }
}

fn run() -> Result<(), String> {
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    let number = env::var("DEP_OPENSSL_VERSION_NUMBER")
        .map_err(|_| "openssl-sys set no DEP_OPENSSL_VERSION_NUMBER: the headers it found are not OpenSSL's (design §7.2)")?;
    let number = i64::from_str_radix(&number, 16)
        .map_err(|error| format!("DEP_OPENSSL_VERSION_NUMBER `{number}`: {error}"))?;
    let include = env::var_os("DEP_OPENSSL_INCLUDE").map(PathBuf::from);
    if let Some(include) = &include {
        println!(
            "cargo::rerun-if-changed={}",
            include.join("openssl/opensslv.h").display()
        );
    }
    let text = expand(&out, include.as_deref())?;
    let header = format!(
        "/// `OPENSSL_VERSION_TEXT` of the headers this build compiled against.\n\
         pub(crate) const OPENSSL_VERSION_TEXT: &str = {text:?};\n\
         /// `OPENSSL_VERSION_NUMBER` of the same headers, `0xMNN00PP0L`.\n\
         pub(crate) const OPENSSL_VERSION_NUMBER: i64 = {number:#x};\n"
    );
    fs::write(out.join("header.rs"), header).map_err(|error| format!("header.rs: {error}"))
}

/// `OPENSSL_VERSION_TEXT` as the C preprocessor expands it, its adjacent
/// quoted strings joined.
fn expand(out: &Path, include: Option<&Path>) -> Result<String, String> {
    let source = out.join("opensslv.c");
    fs::write(&source, SOURCE).map_err(|error| format!("opensslv.c: {error}"))?;
    let mut build = cc::Build::new();
    build.file(&source).cargo_metadata(false);
    if let Some(include) = include {
        build.include(include);
    }
    let expanded = build
        .try_expand()
        .map_err(|error| format!("preprocessing <openssl/opensslv.h>: {error}"))?;
    let expanded = String::from_utf8_lossy(&expanded);
    expanded
        .lines()
        .find_map(|line| line.trim().strip_prefix("php_openssl_header"))
        .and_then(quoted)
        .ok_or_else(|| "<openssl/opensslv.h> defines no string OPENSSL_VERSION_TEXT".to_owned())
}

/// The adjacent quoted C strings of `tokens` as one string, as the
/// compiler joins them; `None` where anything else is there, or an escape
/// other than `\\` and `\"`.
fn quoted(tokens: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = tokens.trim().chars();
    let mut any = false;
    loop {
        match chars.find(|c| !c.is_whitespace()) {
            None => return any.then_some(out),
            Some('"') => any = true,
            Some(_) => return None,
        }
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    escaped @ ('\\' | '"') => out.push(escaped),
                    _ => return None,
                },
                c => out.push(c),
            }
        }
    }
}
