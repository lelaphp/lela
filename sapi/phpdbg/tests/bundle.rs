//! The binary carries the CLI's extensions: php's configure builds an
//! extension of the `cli` class for the CLI, CGI, embed and phpdbg alike
//! (`EXT_CLI_STATIC`, `Z/build/php.m4`) and links phpdbg from the CLI's
//! objects, so the two hosts' `php_bundle` features are one list (#1450).

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
    let phpdbg = include_str!("../Cargo.toml");
    let cli = include_str!("../../cli/Cargo.toml");
    assert_eq!(bundle_features(phpdbg), bundle_features(cli));
}

#[test]
fn the_cli_class_extensions_are_loaded() {
    let dir = std::env::temp_dir().join(format!("sapi-phpdbg-bundle-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.php");
    std::fs::write(
        &file,
        "<?php echo implode(\",\", get_loaded_extensions()), \"\\n\";\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_phpdbg"))
        .args(["-qrr", "-n"])
        .arg(&file)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let loaded = String::from_utf8_lossy(&out.stdout);
    let loaded: Vec<&str> = loaded.trim_end().split(',').collect();
    for name in [
        "curl",
        "exif",
        "shmop",
        "sysvsem",
        "sysvmsg",
        "sysvshm",
        "ftp",
        "mysqlnd",
        "pdo_mysql",
        "mysqli",
    ] {
        assert!(loaded.contains(&name), "{name} in {loaded:?}");
    }
}
