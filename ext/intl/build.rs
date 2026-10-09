//! ICU is the keg the oracle links, icu4c@78 (design §7.2, revision 75;
//! #1573): libicuuc, libicui18n and libicuio, found through `pkg-config`
//! (`icu-uc`, `icu-i18n`, `icu-io`) and linked as the shared libraries
//! php links, never vendored. The keg is keg-only, so its `pkgconfig/`
//! is put ahead of the search path, the keg the oracle links and not a
//! newer `icu4c`; its directory is asked of `brew --prefix`, never written
//! as a path, so the build finds it under whatever prefix Homebrew was
//! installed to. A host without `brew` or the keg falls through to its own
//! search, and a host `pkg-config` finds no ICU on fails the build.
//!
//! ICU renames every C symbol with its major version (`u_errorName_78`);
//! the major is handed to the binding as `INTL_ICU_SUFFIX`, so a pin bump
//! moves the names with the library.
//!
//! `src/ffi/shim/*.cpp` is the part of php's intl written in C++ over
//! ICU's C++ classes, behind `extern "C"`, compiled here by `cc` in C++
//! against the same headers.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The keg the oracle links (`otool -L` on the pinned binary), as
/// Homebrew names its formula.
const KEG: &str = "icu4c@78";

/// The three libraries, as `pkg-config` names them.
const MODULES: [&str; 3] = ["icu-uc", "icu-i18n", "icu-io"];

/// `brew --prefix <formula>`'s `lib/pkgconfig`: the keg's `pkg-config`
/// directory where `brew` runs and answers; `None` where it does not.
fn keg_pkgconfig(formula: &str) -> Option<PathBuf> {
    let output = Command::new("brew")
        .args(["--prefix", formula])
        .output()
        .ok()?;
    let prefix = String::from_utf8(output.stdout).ok()?;
    let prefix = prefix.trim();
    (output.status.success() && !prefix.is_empty()).then(|| Path::new(prefix).join("lib/pkgconfig"))
}

/// `pkg-config <args>` with the keg's directory, `keg`, first, its stdout.
fn pkg_config(keg: Option<&Path>, args: &[&str]) -> Option<String> {
    let rest = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
    let path = match keg {
        Some(keg) if rest.is_empty() => keg.display().to_string(),
        Some(keg) => format!("{}:{rest}", keg.display()),
        None => rest,
    };
    let output = Command::new("pkg-config")
        .env("PKG_CONFIG_PATH", path)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/ffi/shim");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    let keg = keg_pkgconfig(KEG);
    let keg = keg.as_deref();
    let Some(version) = pkg_config(keg, &["--modversion", "icu-uc"]) else {
        println!(
            "cargo::error=pkg-config icu-uc: not found; install icu4c@78, the ICU the oracle links (design §7.2)"
        );
        return;
    };
    let major = version.split('.').next().unwrap_or_default();
    println!("cargo::rustc-env=INTL_ICU_SUFFIX=_{major}");
    let mut args = vec!["--libs-only-L"];
    args.extend(MODULES);
    for flag in pkg_config(keg, &args)
        .unwrap_or_default()
        .split_whitespace()
    {
        if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo::rustc-link-search=native={dir}");
        }
    }
    for library in ["icuio", "icui18n", "icuuc"] {
        println!("cargo::rustc-link-lib=dylib={library}");
    }
    let mut build = cc::Build::new();
    // php compiles C and C++ with -DNDEBUG unless it is built with
    // --enable-debug or --enable-debug-assertions (Z/configure.ac:840), the
    // two builds that set ZEND_DEBUG, as debug assertions are this build's:
    // assert() is live exactly where Rust's debug_assertions is (#1903).
    if std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        build.define("NDEBUG", None);
    }
    build.cpp(true).std("c++17").warnings(true);
    let mut args = vec!["--cflags-only-I"];
    args.extend(MODULES);
    for flag in pkg_config(keg, &args)
        .unwrap_or_default()
        .split_whitespace()
    {
        if let Some(dir) = flag.strip_prefix("-I") {
            build.include(dir);
        }
    }
    let mut shims: Vec<_> = std::fs::read_dir(Path::new("src/ffi/shim"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "cpp"))
        .collect();
    shims.sort();
    build.files(shims).compile("php_intl_shim");
}
