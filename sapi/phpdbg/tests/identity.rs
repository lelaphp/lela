//! What phpdbg's `-V` says it is, in the wording Mitchell ruled on 21
//! November 2026, as `sapi/cli`'s `tests/identity.rs` holds the CLI's:
//! phpdbg's version before php's head line, the line a program reading
//! php's version parses, with Lela's version after it, then what Lela
//! is and whose the derived parts are, and no line naming the Zend
//! Engine or claiming php's copyright as the product's. Held on the
//! binary; the oracle prints php's lines, so `lines.toml` cannot hold
//! these.

use std::process::{Command, Stdio};

/// The provider the build named, as the engine's `build.rs` reads the
/// same variable: none where it is unset or empty.
fn provider() -> Option<&'static str> {
    option_env!("PHP_BUILD_PROVIDER").filter(|provider| !provider.is_empty())
}

/// `text` with the build's date, which is each build's own, as `<date>`.
fn dated(text: &str) -> String {
    let Some((head, rest)) = text.split_once("(built: ") else {
        panic!("no build date in {text}");
    };
    let (_, tail) = rest.split_once(')').unwrap_or_default();
    format!("{head}(built: <date>){tail}")
}

#[test]
fn the_version_is_phpdbgs_and_phps_first_line_with_lelas_version_and_lelas_lines_after_it() {
    let out = Command::new(env!("CARGO_BIN_EXE_lela-dbg"))
        .args(["-n", "-V"])
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running lela-dbg: {e}"));
    assert!(out.status.success(), "{out:?}");
    let php = php_sapi::PHP_VERSION;
    let lela = env!("CARGO_PKG_VERSION");
    let built = provider()
        .map(|p| format!("Built by {p}\n"))
        .unwrap_or_default();
    assert_eq!(
        dated(&String::from_utf8_lossy(&out.stdout)),
        format!(
            "phpdbg {php}, PHP {php} (phpdbg) (built: <date>) (NTS) [Lela {lela}]\n\
             Lela, an independent implementation of PHP {php}. Copyright (c) 2026 Mitchell van Wijngaarden and contributors.\n\
             Parts derived from php-src (Copyright (c) The PHP Group and Contributors, and Zend Technologies Ltd.); see `lela --licenses`.\n\
             Not affiliated with or endorsed by The PHP Group, the PHP Foundation or Perforce.\n\
             {built}"
        )
    );
}
