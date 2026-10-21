//! The binary carries the CLI's extensions: php's configure builds every
//! extension outside the `cli` class for every host (#1477), and design
//! §7.2 (revision 66) puts `pcntl` and `posix` in every host whose serving
//! process is single-threaded, as php-fpm's children are (#1494); the host
//! turns on the bundle's `host` (#3081), and `php-fpm -n -m` lists what it
//! carries.

/// `php-fpm -n -m` lists the extensions a build of this crate alone
/// lacked, `pcntl` and `posix` among them.
#[test]
fn the_cli_class_and_the_rest_are_loaded() {
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
        "pcntl",
        "posix",
        "cgi-fcgi",
    ] {
        assert!(listed.contains(&name), "{name} in {listed:?}");
    }
}
