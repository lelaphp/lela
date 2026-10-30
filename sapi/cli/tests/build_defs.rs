//! The build's facts, one answer each (#1299): the date `php -v` prints
//! after `built:`, `PHP_BUILD_DATE` and phpinfo's `Build Date` are one
//! stamp; `PHP_BUILD_PROVIDER`, `-v`'s `Built by` line and phpinfo's
//! `Build Provider` row stand where the build named a provider and are
//! absent where it did not; and `Build System` and `Configure Command`
//! are this binary's, the target it was built for and the bundle's
//! switches. Held on the binary, since `-v` and `-i` are the host's.

use std::process::{Command, Stdio};

/// What `php -n <args>` writes to stdout.
fn php(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The value of phpinfo's row `name`, where `php -i` prints one.
fn info_row(info: &str, name: &str) -> Option<String> {
    let head = format!("{name} => ");
    info.lines()
        .find_map(|line| line.strip_prefix(&head))
        .map(str::to_owned)
}

/// The provider the build named, as the engine's `build.rs` reads the
/// same variable: none where it is unset or empty.
fn provider() -> Option<&'static str> {
    option_env!("PHP_BUILD_PROVIDER").filter(|provider| !provider.is_empty())
}

#[test]
fn the_version_line_the_constant_and_phpinfo_carry_one_date() {
    let version = php(&["-v"]);
    let built = version
        .split_once("(built: ")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map_or_else(
            || panic!("no build date in {version}"),
            |(date, _)| date.to_owned(),
        );
    assert_eq!(built.len(), "Jul 28 2026 13:06:52".len(), "{built}");
    assert_eq!(php(&["-r", "echo PHP_BUILD_DATE;"]), built);
    let info = php(&["-i"]);
    assert_eq!(
        info_row(&info, "Build Date").as_deref(),
        Some(built.as_str())
    );
}

#[test]
fn the_provider_stands_where_the_build_named_one() {
    let version = php(&["-v"]);
    let info = php(&["-i"]);
    let constant = php(&[
        "-r",
        "echo defined('PHP_BUILD_PROVIDER') ? PHP_BUILD_PROVIDER : '(undefined)';",
    ]);
    if let Some(provider) = provider() {
        assert!(
            version.contains(&format!("\nBuilt by {provider}\n")),
            "{version}"
        );
        assert_eq!(constant, provider);
        assert_eq!(info_row(&info, "Build Provider").as_deref(), Some(provider));
    } else {
        assert!(!version.contains("Built by"), "{version}");
        assert_eq!(constant, "(undefined)");
        assert_eq!(info_row(&info, "Build Provider"), None, "{info}");
    }
}

#[test]
fn the_build_system_and_the_configure_line_are_this_binarys() {
    let info = php(&["-i"]);
    let system = info_row(&info, "Build System").unwrap_or_default();
    assert!(
        system.contains(std::env::consts::ARCH) && system.contains('-'),
        "a target triple: {system}"
    );
    let configure = info_row(&info, "Configure Command").unwrap_or_default();
    for switch in [
        "'--enable-mbstring'",
        "'--with-openssl'",
        "'--enable-pcntl'",
    ] {
        assert!(configure.contains(switch), "{switch} in {configure}");
    }
    assert!(!configure.contains("./configure"), "{configure}");
    assert!(!configure.contains("--prefix"), "{configure}");
}
