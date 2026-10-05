//! libpcre2-8 is the library the pinned keg links (design §7.2, #624,
//! #717): `pcre2-sys` finds it through `pkg-config libpcre2-8` and links
//! it, the keg's archive where the workspace sets `LIBPCRE2_8_STATIC`.
//! Where `pkg-config` misses, `pcre2-sys` builds the source it bundles, a
//! release the oracle does not link; the same lookup here, emitting
//! nothing, fails the build there instead.
//!
//! `PCRE_JIT_SUPPORT` is php's `HAVE_PCRE_JIT_SUPPORT`, which its
//! `configure` decides at build, not the library once loaded (#1958,
//! #1974): `Z/ext/pcre/config0.m4` compiles a program that asks the
//! external library `pcre2_config_8(PCRE2_CONFIG_JIT)`, starts it, and
//! defines the macro where it exits 0. The same program is compiled and
//! started here, against the library `pkg-config` found, and the cfg
//! `pcre_jit` set where it exits 0; a program that does not build or
//! start is a no, as `AC_RUN_IFELSE` takes it. Cross-compiling, where the
//! program cannot start on the build host, the answer is `configure`'s:
//! yes on the CPUs its `host_cpu` list names.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

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
    let jit = jit(&library).unwrap_or_else(|why| {
        println!("cargo::warning=whether external PCRE2 library has JIT supported: no ({why})");
        false
    });
    if jit {
        println!("cargo::rustc-cfg=pcre_jit");
    }
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
    let mut build = cc::Build::new();
    build.cargo_metadata(false);
    for include in &library.include_paths {
        build.include(include);
    }
    for (name, value) in &library.defines {
        build.define(name, value.as_deref());
    }
    let compiler = build
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
