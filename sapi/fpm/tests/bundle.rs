//! The binary carries the CLI's extensions less `pcntl` and `posix`:
//! php's configure builds every extension outside the `cli` class for
//! every host, and design §7.2 (revision 35) keeps those two off a server
//! host (#1477).

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
fn the_bundle_is_the_clis_less_pcntl_and_posix() {
    let fpm = include_str!("../Cargo.toml");
    let cli = include_str!("../../cli/Cargo.toml");
    let cli: Vec<&str> = bundle_features(cli)
        .into_iter()
        .filter(|feature| !matches!(*feature, "pcntl" | "posix"))
        .collect();
    assert_eq!(bundle_features(fpm), cli);
}

/// `php-fpm -n -m` lists the extensions a build of this crate alone
/// lacked, and neither of the two a server host does not carry.
#[test]
fn the_rest_are_loaded_and_pcntl_and_posix_are_not() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-n", "-m"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&out.stdout);
    let listed: Vec<&str> = listed.lines().collect();
    for name in [
        "curl",
        "exif",
        "ftp",
        "mbstring",
        "mysqli",
        "mysqlnd",
        "pdo_mysql",
        "shmop",
        "sysvmsg",
        "sysvsem",
        "sysvshm",
        "cgi-fcgi",
    ] {
        assert!(listed.contains(&name), "{name} in {listed:?}");
    }
    for name in ["pcntl", "posix"] {
        assert!(!listed.contains(&name), "{name} in {listed:?}");
    }
}
