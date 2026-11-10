//! ICU is the library the oracle links (design §7.2, revision 75; #1573):
//! libicuuc, libicui18n and libicuio, found through `pkg-config`
//! (`icu-uc`, `icu-i18n`, `icu-io`) and linked as the shared libraries
//! php links, never vendored. Where it lives is the environment's to say,
//! through `PKG_CONFIG_PATH`, which the Mac runner's environment and the
//! developer's shell set (Homebrew's `icu4c@<major>` kegs are keg-only;
//! #2486); the build writes no path of its own. What it holds is the
//! version: the major is the one the pin's `[closure]` names in its
//! `icu4c@<major>` keg, read from `oracle.toml` when the build runs
//! (`build/pin.rs`, #4406), so a bump that re-freezes the closure on
//! another ICU moves the build with it. An ICU of another major fails
//! the build naming the keg the pin asks for, as does no ICU at all
//! (#2278), and so does a pin that names no one ICU keg.
//!
//! ICU renames every C symbol with its major version
//! (`u_errorName_<major>`); the major is handed to the binding as
//! `INTL_ICU_SUFFIX`, so a pin bump moves the names with the library.
//!
//! `src/ffi/shim/*.cpp` is the part of php's intl written in C++ over
//! ICU's C++ classes, behind `extern "C"`, compiled here by `cc` in C++
//! against the same headers.

#[path = "build/pin.rs"]
mod pin;

use std::path::Path;
use std::process::Command;

/// The three libraries, as `pkg-config` names them.
const MODULES: [&str; 3] = ["icu-uc", "icu-i18n", "icu-io"];

/// `pkg-config <args>` on the environment's search path, its stdout.
fn pkg_config(args: &[&str]) -> Option<String> {
    let output = Command::new("pkg-config").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=build/pin.rs");
    println!("cargo::rerun-if-changed=src/ffi/shim");
    println!("cargo::rerun-if-env-changed=PKG_CONFIG_PATH");
    // Read when `build.rs` runs, not when it compiles: cargo runs one
    // compiled `build.rs` for every checkout sharing a target directory
    // (#2628).
    let Some(dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        println!("cargo::error=CARGO_MANIFEST_DIR is unset; cargo sets it for every `build.rs`");
        return;
    };
    let path = Path::new(&dir).join("../../oracle.toml");
    println!("cargo::rerun-if-changed={}", path.display());
    let pinned = match std::fs::read_to_string(&path) {
        Ok(text) => pin::icu_major(&text).map_err(|refusal| refusal.to_string()),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    };
    let wanted = match pinned {
        Ok(major) => major,
        Err(why) => {
            println!("cargo::error={why}");
            return;
        }
    };
    let keg = wanted.keg();
    let Some(version) = pkg_config(&["--modversion", "icu-uc"]) else {
        println!(
            "cargo::error=pkg-config icu-uc: no ICU found; the pin asks for {keg}: put its lib/pkgconfig on PKG_CONFIG_PATH (Homebrew: `$(brew --prefix {keg})/lib/pkgconfig`)"
        );
        return;
    };
    let found = version.split('.').next().and_then(|m| m.parse().ok());
    if found.map(pin::Major) != Some(wanted) {
        println!(
            "cargo::error=pkg-config icu-uc: found ICU {version}, where the pin asks for {keg}: put its lib/pkgconfig ahead on PKG_CONFIG_PATH (Homebrew: `$(brew --prefix {keg})/lib/pkgconfig`)"
        );
        return;
    }
    println!("cargo::rustc-env=INTL_ICU_SUFFIX=_{wanted}");
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
