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
//! cannot be read fails the build. Beside them goes whether the headers
//! define `OPENSSL_NO_SSL3`, which `xp_ssl.c` and `openssl.c` test with
//! `#ifndef` (`HAVE_SSL3`, and `sslv3` among the registered names): OpenSSL 3's
//! configuration defines it and OpenSSL 4's does not.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// The line the preprocessor expands: the marker, then the macro.
const SOURCE: &str = "#include <openssl/opensslv.h>\n#include <openssl/opensslconf.h>\n\
#ifdef OPENSSL_NO_SSL3\nphp_openssl_ssl3 0\n#else\nphp_openssl_ssl3 1\n#endif\n\
php_openssl_header OPENSSL_VERSION_TEXT\n";

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
    let number = u32::from_str_radix(&number, 16)
        .map_err(|error| format!("DEP_OPENSSL_VERSION_NUMBER `{number}`: {error}"))?;
    let include = env::var_os("DEP_OPENSSL_INCLUDE").map(PathBuf::from);
    let Headers { text, ssl3 } = expand(&out, include.as_deref())?;
    let (high, low) = (number >> 16, number & 0xffff);
    let header = format!(
        "/// `OPENSSL_VERSION_TEXT` of the headers this build compiled against.\n\
         pub(crate) const OPENSSL_VERSION_TEXT: &str = {text:?};\n\
         /// `OPENSSL_VERSION_NUMBER` of the same headers, `0xMNN00PP0L`.\n\
         pub(crate) const OPENSSL_VERSION_NUMBER: i64 = 0x{high:04x}_{low:04x};\n\
         /// `HAVE_SSL3`: the same headers leave `OPENSSL_NO_SSL3` undefined.\n\
         pub(crate) const HAVE_SSL3: bool = {ssl3};\n"
    );
    fs::write(out.join("header.rs"), header).map_err(|error| format!("header.rs: {error}"))
}

/// What the headers say, as the C preprocessor reads them.
struct Headers {
    /// `OPENSSL_VERSION_TEXT`, its adjacent quoted strings joined.
    text: String,
    /// Whether `OPENSSL_NO_SSL3` is left undefined.
    ssl3: bool,
}

/// [`Headers`] as the C preprocessor expands them. The `opensslv.h` and
/// the configuration header it read, named by its line markers, are
/// watched, so that a changed header rebuilds the crate as it would
/// rebuild php.
fn expand(out: &Path, include: Option<&Path>) -> Result<Headers, String> {
    let source = out.join("opensslv.c");
    fs::write(&source, SOURCE).map_err(|error| format!("opensslv.c: {error}"))?;
    let mut build = cc::Build::new();
    // php compiles C and C++ with -DNDEBUG unless it is built with
    // --enable-debug or --enable-debug-assertions (Z/configure.ac:840), the
    // two builds that set ZEND_DEBUG, as debug assertions are this build's:
    // assert() is live exactly where Rust's debug_assertions is (#1903).
    if std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        build.define("NDEBUG", None);
    }
    build.file(&source).cargo_metadata(false);
    if let Some(include) = include {
        build.include(include);
    }
    let expanded = build
        .try_expand()
        .map_err(|error| format!("preprocessing <openssl/opensslv.h>: {error}"))?;
    let expanded = String::from_utf8_lossy(&expanded);
    let read: BTreeSet<&str> = expanded
        .lines()
        .filter(|line| line.starts_with('#'))
        .filter_map(|line| line.split('"').nth(1))
        .filter(|path| {
            ["/opensslv.h", "/opensslconf.h", "/configuration.h"]
                .iter()
                .any(|name| path.ends_with(name))
        })
        .collect();
    for path in read {
        println!("cargo::rerun-if-changed={path}");
    }
    let text = after_marker(&expanded, "php_openssl_header")
        .as_deref()
        .and_then(quoted)
        .ok_or_else(|| "<openssl/opensslv.h> defines no string OPENSSL_VERSION_TEXT".to_owned())?;
    let ssl3 = match after_marker(&expanded, "php_openssl_ssl3")
        .as_deref()
        .and_then(|rest| rest.split_whitespace().next())
    {
        Some("1") => true,
        Some("0") => false,
        _ => return Err("the OPENSSL_NO_SSL3 probe did not expand".to_owned()),
    };
    Ok(Headers { text, ssl3 })
}

/// The adjacent quoted C strings of `tokens` as one string, as the
/// compiler joins them; `None` where anything else is there, or an escape.
fn quoted(tokens: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = tokens.trim();
    while let Some(open) = rest.strip_prefix('"') {
        let (text, after) = open.split_once('"')?;
        if text.contains('\\') {
            return None;
        }
        out.push_str(text);
        rest = after.trim_start();
    }
    (rest.is_empty() && !out.is_empty()).then_some(out)
}

/// The tokens the preprocessor wrote after `marker`, the line markers
/// dropped and the rest joined: GCC breaks a line where a macro from a
/// system header expands, writing `# 2 "file" 3 4` before the expansion,
/// where clang keeps it on the marker's line.
fn after_marker(expanded: &str, marker: &str) -> Option<String> {
    let joined = expanded
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ");
    let at = joined.rfind(marker)?;
    Some(joined[at + marker.len()..].trim().to_owned())
}
