//! libintl is the keg's GNU gettext (design §7.2, revision 72),
//! the library the oracle links (`--with-gettext=$(brew --prefix
//! gettext)`), never vendored; macOS's C library carries none. The keg
//! ships no `pkg-config` file for libintl, so the directory is found as
//! php's `config.m4` finds it: the first of the one named (`GETTEXT_DIR`,
//! as `--with-gettext=DIR` names it, then the keg's, as `brew --prefix
//! gettext` answers where `brew` runs), `/usr/local` and `/usr` that holds
//! `include/libintl.h`. The keg is asked for, never written as a path, so
//! the build finds it under whatever prefix Homebrew was installed to. Where its `lib` holds a
//! libintl, `-l intl` links the shared library as the oracle links
//! `libintl.8.dylib`: neither the SDK nor its `/usr/lib` ships one, so
//! no link order finds another. Where it holds none, the C library
//! carries the functions, as glibc's does (`AC_CHECK_LIB([c], ...)`). A
//! host with no `libintl.h` on the list fails the build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=GETTEXT_DIR");
    let named = std::env::var_os("GETTEXT_DIR").map(PathBuf::from);
    let candidates = named
        .into_iter()
        .chain(keg("gettext"))
        .chain(["/usr/local", "/usr"].map(PathBuf::from));
    let Some(dir) = candidates
        .into_iter()
        .find(|dir| dir.join("include/libintl.h").is_file())
    else {
        println!(
            "cargo::error=Cannot locate header file libintl.h; install GNU gettext, the libintl the oracle links, Homebrew's `gettext` keg on a Mac (design §7.2)"
        );
        return;
    };
    let lib = dir.join("lib");
    let carried = ["libintl.dylib", "libintl.so", "libintl.a"]
        .iter()
        .any(|name| Path::new(&lib).join(name).is_file());
    if carried {
        println!("cargo::rustc-link-search=native={}", lib.display());
        println!("cargo::rustc-link-lib=dylib=intl");
    }
}

/// `brew --prefix <formula>`: the keg's directory where `brew` runs and
/// answers, whether or not the keg is installed; `None` where it does not.
fn keg(formula: &str) -> Option<PathBuf> {
    let output = Command::new("brew")
        .args(["--prefix", formula])
        .output()
        .ok()?;
    let prefix = String::from_utf8(output.stdout).ok()?;
    let prefix = prefix.trim();
    (output.status.success() && !prefix.is_empty()).then(|| PathBuf::from(prefix))
}
