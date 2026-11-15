//! The two constants php registers from `<zlib.h>`, `ZLIB_VERSION` and
//! `ZLIB_VERNUM` (`@cvalue` in `Z/ext/zlib/zlib.stub.php`), are the values
//! of the header a build compiles against, not the zlib it later loads.
//! They are read here, as the C preprocessor expands the two
//! macros of the `<zlib.h>` a C build finds: under the include
//! directories libz-sys hands a dependent as `DEP_Z_INCLUDE` where it
//! names any, else the compiler's own, which for the platform's zlib is
//! the SDK's or the system's header. Both are written to
//! `$OUT_DIR/header.rs` as Rust constants, and a header that cannot be
//! read fails the build.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;

/// The line the preprocessor expands: the marker, then the two macros.
const SOURCE: &str = "#include <zlib.h>\nphp_zlib_header ZLIB_VERSION ZLIB_VERNUM\n";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if let Err(error) = run() {
        println!("cargo::error={error}");
    }
}

fn run() -> Result<(), String> {
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    let source = out.join("zlib_h.c");
    fs::write(&source, SOURCE).map_err(|error| format!("zlib_h.c: {error}"))?;
    let mut build = cc::Build::new();
    // php compiles C and C++ with -DNDEBUG unless it is built with
    // --enable-debug or --enable-debug-assertions (Z/configure.ac:840), the
    // two builds that set ZEND_DEBUG, as debug assertions are this build's:
    // assert() is live exactly where Rust's debug_assertions is.
    if std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        build.define("NDEBUG", None);
    }
    build.file(&source).cargo_metadata(false);
    let includes = env::var("DEP_Z_INCLUDE").unwrap_or_default();
    for include in includes.split(',').filter(|dir| !dir.is_empty()) {
        build.include(include);
    }
    let expanded = build
        .try_expand()
        .map_err(|error| format!("preprocessing <zlib.h>: {error}"))?;
    let expanded = String::from_utf8_lossy(&expanded);
    // The header the preprocessor read, from its line markers, so that a
    // changed header rebuilds the crate as it would rebuild php.
    let read: BTreeSet<&str> = expanded
        .lines()
        .filter(|line| line.starts_with('#'))
        .filter_map(|line| line.split('"').nth(1))
        .filter(|path| path.ends_with("/zlib.h"))
        .collect();
    for path in read {
        println!("cargo::rerun-if-changed={path}");
    }
    let (version, vernum) = after_marker(&expanded, "php_zlib_header")
        .as_deref()
        .and_then(macros)
        .ok_or("<zlib.h> defines no string ZLIB_VERSION and integer ZLIB_VERNUM")?;
    let header = format!(
        "/// `ZLIB_VERSION` of the `zlib.h` this build compiled against.\n\
         pub(crate) const ZLIB_VERSION: &str = {version:?};\n\
         /// `ZLIB_VERNUM` of the same header, `0xMmrs`.\n\
         pub(crate) const ZLIB_VERNUM: i64 = {vernum:#x};\n"
    );
    fs::write(out.join("header.rs"), header).map_err(|error| format!("header.rs: {error}"))
}

/// The expansion of `ZLIB_VERSION ZLIB_VERNUM`: one quoted C string with
/// no escape, then one integer, hexadecimal or decimal, with any suffix.
fn macros(tokens: &str) -> Option<(String, i64)> {
    let rest = tokens.trim().strip_prefix('"')?;
    let (version, rest) = rest.split_once('"')?;
    if version.contains('\\') {
        return None;
    }
    let number = rest
        .split_whitespace()
        .next()?
        .trim_end_matches(['u', 'U', 'l', 'L']);
    let vernum = match number
        .strip_prefix("0x")
        .or_else(|| number.strip_prefix("0X"))
    {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => number.parse().ok()?,
    };
    Some((version.to_owned(), vernum))
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
