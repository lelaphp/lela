//! What the CLI says it is, in the wording Mitchell ruled on 21
//! November 2026: `-v`'s first line in php's shape, the line a program
//! reading php's version parses, with Lela's version after it, then
//! what Lela is and whose the derived parts are, and no line naming the
//! Zend Engine or claiming php's copyright as the product's. Held on the
//! binary, since it is the host's. The oracle prints php's lines, so
//! `lines.toml` cannot hold these.

use std::process::{Command, Output, Stdio};

/// What `lela <args>` gave.
fn lela(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lela"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running lela: {e}"))
}

/// What `lela -n -r <code>` wrote.
fn echoed(code: &str) -> String {
    String::from_utf8_lossy(&lela(&["-n", "-r", code]).stdout).into_owned()
}

/// The provider the build named, as the engine's `build.rs` reads the
/// same variable: none where it is unset or empty.
fn provider() -> Option<&'static str> {
    option_env!("PHP_BUILD_PROVIDER").filter(|provider| !provider.is_empty())
}

#[test]
fn the_version_is_phps_first_line_with_lelas_version_and_lelas_lines_after_it() {
    let out = lela(&["-n", "-v"]);
    assert!(out.status.success(), "{out:?}");
    let php = echoed("echo PHP_VERSION;");
    let date = echoed("echo PHP_BUILD_DATE;");
    let lela = env!("CARGO_PKG_VERSION");
    let built = provider()
        .map(|p| format!("Built by {p}\n"))
        .unwrap_or_default();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "PHP {php} (cli) (built: {date}) (NTS) [Lela {lela}]\n\
             Lela, an independent implementation of PHP {php}. Copyright (c) 2026 Mitchell van Wijngaarden and contributors.\n\
             Parts derived from php-src (Copyright (c) The PHP Group and Contributors, and Zend Technologies Ltd.); see `lela --licenses`.\n\
             Not affiliated with or endorsed by The PHP Group, the PHP Foundation or Perforce.\n\
             {built}"
        )
    );
}

/// phpinfo's general table carries the version `-v` does, the package's,
/// beside php's heading.
#[test]
fn phpinfo_carries_lelas_version_beside_phps() {
    let info = String::from_utf8_lossy(&lela(&["-n", "-i"]).stdout).into_owned();
    assert!(
        info.starts_with(&format!(
            "phpinfo()\nPHP Version => {}\n",
            echoed("echo PHP_VERSION;")
        )),
        "{info}"
    );
    let row = format!("\nLela Version => {}\n", env!("CARGO_PKG_VERSION"));
    assert!(info.contains(&row), "{row} in {info}");
}

/// What a program reads for php's version stays php's.
#[test]
fn the_constants_a_program_reads_are_still_phps() {
    assert_eq!(
        echoed(
            "echo PHP_VERSION, ' ', phpversion(), ' ', zend_version(), ' ', PHP_MAJOR_VERSION, PHP_MINOR_VERSION;"
        ),
        "8.5.12 8.5.12 4.5.12 85"
    );
}
