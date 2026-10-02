//! The binary carries the CLI's extensions: php's configure builds an
//! extension of the `cli` class for the CLI, CGI, embed and phpdbg alike
//! (`EXT_CLI_STATIC`, `Z/build/php.m4`) and every other one for every host,
//! so the two hosts' `php_bundle` features are one list (#1477).

/// The `features` a host's manifest turns on in its `php_bundle`
/// dependency, as written on its one line.
fn bundle_features(manifest: &str) -> Vec<&str> {
    let line = manifest
        .lines()
        .find(|line| line.starts_with("php_bundle = "))
        .expect("a php_bundle dependency on one line");
    let (_, list) = line
        .split_once("features = [")
        .expect("the dependency names its features");
    let (list, _) = list.split_once(']').expect("the list closes on its line");
    list.split(',')
        .map(|feature| feature.trim().trim_matches('"'))
        .filter(|feature| !feature.is_empty())
        .collect()
}

#[test]
fn the_bundle_is_the_clis() {
    let cgi = include_str!("../Cargo.toml");
    let cli = include_str!("../../cli/Cargo.toml");
    assert_eq!(bundle_features(cgi), bundle_features(cli));
}

/// `php-cgi -n -m` lists the ten a build of this crate alone lacked while
/// its features were the CLI's first eight.
#[test]
fn the_cli_class_and_the_rest_are_loaded() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .env_clear()
        .args(["-n", "-m"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&out.stdout);
    let listed: Vec<&str> = listed.lines().collect();
    for name in [
        "curl",
        "exif",
        "ftp",
        "mysqli",
        "mysqlnd",
        "pdo_mysql",
        "shmop",
        "sysvmsg",
        "sysvsem",
        "sysvshm",
        "pcntl",
        "posix",
        "cgi-fcgi",
    ] {
        assert!(listed.contains(&name), "{name} in {listed:?}");
    }
}
