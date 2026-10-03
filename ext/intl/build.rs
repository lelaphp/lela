//! ICU is the keg the oracle links, icu4c@78 (design §7.2, revision 75;
//! #1573): libicuuc, libicui18n and libicuio, found through `pkg-config`
//! (`icu-uc`, `icu-i18n`, `icu-io`) and linked as the shared libraries
//! php links, never vendored. The keg is keg-only, so its `pkgconfig/`
//! is put ahead of the search path, the keg the oracle links and not a
//! newer `icu4c`; a host without it falls through to its own search, and
//! a host `pkg-config` finds no ICU on fails the build.
//!
//! ICU renames every C symbol with its major version (`u_errorName_78`);
//! the major is handed to the binding as `INTL_ICU_SUFFIX`, so a pin bump
//! moves the names with the library.
//!
//! `src/ffi/shim/*.cpp` is the part of php's intl written in C++ over
//! ICU's C++ classes, behind `extern "C"`, compiled here by `cc` in C++
//! against the same headers.

use std::path::Path;
use std::process::Command;

/// The keg the oracle links (`otool -L` on the pinned binary).
const KEG: &str = "/opt/homebrew/opt/icu4c@78/lib/pkgconfig";

/// The three libraries, as `pkg-config` names them.
const MODULES: [&str; 3] = ["icu-uc", "icu-i18n", "icu-io"];

/// `pkg-config <args>` with the keg's directory first, its stdout.
fn pkg_config(args: &[&str]) -> Option<String> {
    let path = match std::env::var("PKG_CONFIG_PATH") {
        Ok(rest) if !rest.is_empty() => format!("{KEG}:{rest}"),
        _ => KEG.to_owned(),
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
    let Some(version) = pkg_config(&["--modversion", "icu-uc"]) else {
        println!(
            "cargo::error=pkg-config icu-uc: not found; install icu4c@78, the ICU the oracle links (design §7.2)"
        );
        return;
    };
    let major = version.split('.').next().unwrap_or_default();
    println!("cargo::rustc-env=INTL_ICU_SUFFIX=_{major}");
    let mut args = vec!["--libs-only-L"];
    args.extend(MODULES);
    for flag in pkg_config(&args).unwrap_or_default().split_whitespace() {
        if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo::rustc-link-search=native={dir}");
        }
    }
    for library in ["icuio", "icui18n", "icuuc"] {
        println!("cargo::rustc-link-lib=dylib={library}");
    }
    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").warnings(true);
    let mut args = vec!["--cflags-only-I"];
    args.extend(MODULES);
    for flag in pkg_config(&args).unwrap_or_default().split_whitespace() {
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
