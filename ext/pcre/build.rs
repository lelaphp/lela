//! libpcre2-8 is the library the pinned keg links (design §7.2):
//! `pcre2-sys` finds it through `pkg-config libpcre2-8` and links
//! it, the keg's archive where the workspace sets `LIBPCRE2_8_STATIC`.
//! Where `pkg-config` misses, `pcre2-sys` builds the source it bundles, a
//! release the oracle does not link; the same lookup here, emitting
//! nothing, fails the build there instead.
//!
//! `PCRE_JIT_SUPPORT` is php's `HAVE_PCRE_JIT_SUPPORT`, which its
//! `configure` decides at build, not the library once loaded:
//! `Z/ext/pcre/config0.m4` compiles a program that asks the
//! external library `pcre2_config_8(PCRE2_CONFIG_JIT)`, starts it, and
//! defines the macro where it exits 0. The same program is compiled and
//! started here, against the library `pkg-config` found, and the cfg
//! `pcre_jit` set where it exits 0; a program that does not build or
//! start is a no, as `AC_RUN_IFELSE` takes it. Cross-compiling, where the
//! program cannot start on the build host, the answer is `configure`'s:
//! yes on the CPUs its `host_cpu` list names.
//!
//! `PCRE_VERSION_MAJOR` and `PCRE_VERSION_MINOR` are `@cvalue PCRE2_MAJOR`
//! and `@cvalue PCRE2_MINOR` (`Z/ext/pcre/php_pcre.stub.php`), the header
//! a build compiles against, not the library it later loads (as the other
//! extensions' header macros are read). They are read here, as the C
//! preprocessor expands the two macros of the `pcre2.h` that
//! `config0.m4`'s program compiles against, under the include directories
//! `pkg-config` found, never from `pcre2-sys`, whose pregenerated bindings
//! carry a header of their own. Both are written to `$OUT_DIR/header.rs` as Rust constants,
//! and a header that cannot be read fails the build.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// The lines the preprocessor expands: the marker, then the two macros.
const HEADER: &str = "#define PCRE2_CODE_UNIT_WIDTH 8
#include <pcre2.h>
php_pcre2_header PCRE2_MAJOR PCRE2_MINOR
";

/// `config0.m4`'s program, its exit 0 where the library was built with
/// `SUPPORT_JIT`.
const PROGRAM: &str = "#define PCRE2_CODE_UNIT_WIDTH 8
#include <pcre2.h>
#include <stdlib.h>
int main(void) {
  uint32_t have_jit;
  pcre2_config_8(PCRE2_CONFIG_JIT, &have_jit);
  return !have_jit;
}
";

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(pcre_jit)");
    let library = match pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libpcre2-8")
    {
        Ok(library) => library,
        Err(error) => {
            println!(
                "cargo::error=pkg-config libpcre2-8: {error}; install the libpcre2-8 the oracle links, Homebrew's `pcre2` keg on a Mac (design §7.2)"
            );
            return;
        }
    };
    if let Err(error) = header(&library) {
        println!("cargo::error={error}");
        return;
    }
    let jit = jit(&library).unwrap_or_else(|why| {
        println!("cargo::warning=whether external PCRE2 library has JIT supported: no ({why})");
        false
    });
    if jit {
        println!("cargo::rustc-cfg=pcre_jit");
    }
}

/// A C build against the library `pkg-config` found, as php's
/// `configure` compiles its test programs: its include directories and
/// defines.
fn c_build(library: &pkg_config::Library) -> cc::Build {
    let mut build = cc::Build::new();
    // php compiles C and C++ with -DNDEBUG unless it is built with
    // --enable-debug or --enable-debug-assertions (Z/configure.ac:840), the
    // two builds that set ZEND_DEBUG, as debug assertions are this build's:
    // assert() is live exactly where Rust's debug_assertions is.
    if env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none() {
        build.define("NDEBUG", None);
    }
    build.cargo_metadata(false);
    for include in &library.include_paths {
        build.include(include);
    }
    for (name, value) in &library.defines {
        build.define(name, value.as_deref());
    }
    build
}

/// `PCRE2_MAJOR` and `PCRE2_MINOR` of the `pcre2.h` [`c_build`] finds,
/// written to `$OUT_DIR/header.rs`.
fn header(library: &pkg_config::Library) -> Result<(), String> {
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    let source = out.join("pcre2_h.c");
    fs::write(&source, HEADER).map_err(|error| format!("{}: {error}", source.display()))?;
    let expanded = c_build(library)
        .file(&source)
        .try_expand()
        .map_err(|error| format!("preprocessing <pcre2.h>: {error}"))?;
    let expanded = String::from_utf8_lossy(&expanded);
    // The header the preprocessor read, from its line markers, so that a
    // changed header rebuilds the crate as it would rebuild php.
    let read: BTreeSet<&str> = expanded
        .lines()
        .filter(|line| line.starts_with('#'))
        .filter_map(|line| line.split('"').nth(1))
        .filter(|path| path.ends_with("/pcre2.h"))
        .collect();
    for path in read {
        println!("cargo::rerun-if-changed={path}");
    }
    let (major, minor) = after_marker(&expanded, "php_pcre2_header")
        .as_deref()
        .and_then(numbers)
        .ok_or("<pcre2.h> defines no integer PCRE2_MAJOR and PCRE2_MINOR")?;
    let header = format!(
        "/// `PCRE2_MAJOR` of the `pcre2.h` this build compiled against.\n\
         pub(crate) const PCRE2_MAJOR: i64 = {major};\n\
         /// `PCRE2_MINOR` of the same header.\n\
         pub(crate) const PCRE2_MINOR: i64 = {minor};\n"
    );
    fs::write(out.join("header.rs"), header).map_err(|error| format!("header.rs: {error}"))
}

/// The expansion of `PCRE2_MAJOR PCRE2_MINOR`: two decimal integers.
fn numbers(tokens: &str) -> Option<(i64, i64)> {
    let mut words = tokens.split_whitespace();
    let major = words.next()?.parse().ok()?;
    let minor = words.next()?.parse().ok()?;
    Some((major, minor))
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

/// `AC_RUN_IFELSE` over [`PROGRAM`]: `Ok` with its answer, `Err` with why
/// it did not build or start, which `configure` takes as a no.
fn jit(library: &pkg_config::Library) -> Result<bool, String> {
    if env::var("HOST").ok() != env::var("TARGET").ok() {
        return Ok(host_cpu_answer());
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is unset")?);
    let source = out.join("pcre2_jit.c");
    let program = out.join("pcre2_jit");
    fs::write(&source, PROGRAM).map_err(|error| format!("{}: {error}", source.display()))?;
    let compiler = c_build(library)
        .try_get_compiler()
        .map_err(|error| error.to_string())?;
    if compiler.is_like_msvc() {
        return Ok(host_cpu_answer());
    }
    let mut command = compiler.to_command();
    command.arg(&source).arg("-o").arg(&program);
    for dir in &library.link_paths {
        command.arg(format!("-L{}", dir.display()));
    }
    for lib in &library.libs {
        command.arg(format!("-l{lib}"));
    }
    let status = command
        .status()
        .map_err(|error| format!("the compiler: {error}"))?;
    if !status.success() {
        return Err(format!("the program did not build ({status})"));
    }
    let ran = Command::new(&program)
        .status()
        .map_err(|error| format!("the program: {error}"))?;
    // `return !have_jit`: 0 or 1; any other exit is a program that did
    // not start, the library out of the loader's reach.
    match ran.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!("the program did not start ({ran})")),
    }
}

/// `configure`'s answer where it cannot start the program: `arm*`, `i386`
/// to `i786`, `x86_64`, `mips*`, `powerpc*` and `sparc`, in Rust's names
/// for those CPUs.
fn host_cpu_answer() -> bool {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    matches!(arch.as_str(), "arm" | "x86" | "x86_64" | "sparc")
        || arch.starts_with("mips")
        || arch.starts_with("powerpc")
}
