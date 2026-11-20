//! The binary carries the CLI's extensions: php's configure builds an
//! extension of the `cli` class for the CLI, CGI, embed and phpdbg alike
//! (`EXT_CLI_STATIC`, `Z/build/php.m4`) and every other one for every host,
//! so the host turns on the bundle's `host`, the set every host links,
//! and `php-cgi -n -m` lists what it carries.

/// `php-cgi -n -m` lists the ten a build of this crate alone lacked while
/// its features were the CLI's first eight.
#[test]
fn the_cli_class_and_the_rest_are_loaded() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lela-cgi"))
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
