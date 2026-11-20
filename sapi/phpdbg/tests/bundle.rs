//! The binary carries the CLI's extensions: php's configure builds an
//! extension of the `cli` class for the CLI, CGI, embed and phpdbg alike
//! (`EXT_CLI_STATIC`, `Z/build/php.m4`) and links phpdbg from the CLI's
//! objects, so the host turns on the bundle's `host`, the set every host
//! links, and `get_loaded_extensions()` lists what it
//! carries.

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
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lela-dbg"))
        .args(["-qrr", "-n"])
        .arg(&file)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
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
