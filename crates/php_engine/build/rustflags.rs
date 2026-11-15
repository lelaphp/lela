//! The LLVM option every profile passes, held where the engine is built
//! (design §10 Profiles). `.cargo/config.toml`'s rustflags table
//! passes `-C llvm-args=--cgp-max-address-users-to-scan=1000`, so
//! `CodeGenPrepare` folds a request field's address into each access of
//! `serve_frame`'s loop. A `RUSTFLAGS` variable replaces that table, and
//! a build from outside the workspace never reads it, so the engine's
//! `build.rs` reads the flags cargo hands rustc, `CARGO_ENCODED_RUSTFLAGS`,
//! and refuses a release-like build without the option where the build
//! asks it to, through [`ASK`], which `.cargo/config.toml`'s `[env]` sets
//! for every build in the workspace, and warns of it where none asks. A
//! `dev` build is neither refused nor warned of: no bench reads its code.
//!
//! One file for `php_engine`'s `build.rs` and the harness's negative
//! control, `xtask/fixtures/rustflags`, which builds it as a crate of its
//! own, so the control holds the rule the engine is built under.

use std::env;

/// The LLVM option, as `-C llvm-args` passes it.
pub const OPTION: &str = "--cgp-max-address-users-to-scan";

/// The least value of [`OPTION`] the ruling of 2026-10-24 takes: at 1000
/// every field `serve_frame` reaches is folded, and 100000 measured the
/// same.
pub const LEAST: u64 = 1000;

/// The variable a build asks for the refusal through: `refuse`, or
/// `warn` to build without the option anyway, unfolded.
pub const ASK: &str = "PHP_ENGINE_FOLD";

/// What a release-like build without the option gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// The build fails, naming the option and how to add flags.
    Refuse,
    /// The build goes on with a warning, as one outside the workspace does.
    Warn,
}

impl Ask {
    /// [`ASK`]'s value decoded once: `refuse` or `warn`, and `warn` where
    /// it is unset, as for a build that never reads the workspace's
    /// configuration.
    pub fn of(value: Option<&str>) -> Result<Ask, String> {
        match value {
            Some("refuse") => Ok(Ask::Refuse),
            Some("warn") | None => Ok(Ask::Warn),
            Some(other) => Err(format!(
                "{ASK}={other:?} is neither `refuse` nor `warn` (design §10 Profiles)"
            )),
        }
    }
}

/// The value of [`OPTION`] the flags pass rustc, `encoded` as cargo hands
/// it, one after another with a `0x1f` between: from every `-C llvm-args=` in any of
/// rustc's spellings, the last where several pass it, as LLVM takes the
/// last. `None` where none passes it with a value.
pub fn passed(encoded: &str) -> Option<u64> {
    let flags: Vec<&str> = encoded.split('\x1f').collect();
    let mut found = None;
    for (at, flag) in flags.iter().enumerate() {
        let codegen = match *flag {
            "-C" | "--codegen" => flags.get(at + 1).copied(),
            other => other
                .strip_prefix("-C")
                .or_else(|| other.strip_prefix("--codegen=")),
        };
        let Some(args) = codegen.and_then(|c| c.strip_prefix("llvm-args=")) else {
            continue;
        };
        // LLVM takes an option after one dash or two.
        let name = OPTION.trim_start_matches('-');
        for arg in args.split_whitespace() {
            let named = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'));
            let rest = named.and_then(|named| named.strip_prefix(name));
            if let Some(value) = rest.and_then(|rest| rest.strip_prefix('=')) {
                found = value.parse().ok().or(found);
            }
        }
    }
    found
}

/// What `php_engine`'s `build.rs` runs first: for a release-like build,
/// the profile cargo says inherits `release`, the option held to
/// [`LEAST`], refused or warned of as [`ASK`] says.
pub fn hold() {
    println!("cargo::rerun-if-env-changed={ASK}");
    if !env::var("PROFILE").is_ok_and(|profile| profile == "release") {
        return;
    }
    let ask = match Ask::of(env::var(ASK).ok().as_deref()) {
        Ok(ask) => ask,
        Err(why) => {
            println!("cargo::error={why}");
            return;
        }
    };
    let passed = passed(&env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default());
    if passed.is_some_and(|value| value >= LEAST) {
        return;
    }
    let what = match passed {
        Some(value) => {
            format!("passes rustc LLVM {OPTION}={value}, under the {LEAST} the ruling takes")
        }
        None => format!("passes rustc no LLVM {OPTION}"),
    };
    let said = format!(
        "this release build of php_engine {what}, so serve_frame's accesses to the request may go unfolded \
         (design §10 Profiles); .cargo/config.toml's [target.'cfg(all())'] rustflags passes \
         -C llvm-args={OPTION}={LEAST} to every profile, and a RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS \
         variable replaces that table: add flags with --config \"target.'cfg(all())'.rustflags=[...]\" \
         instead, or set {ASK}=warn to build it unfolded"
    );
    match ask {
        Ask::Refuse => println!("cargo::error={said}"),
        Ask::Warn => println!("cargo::warning={said}"),
    }
}
