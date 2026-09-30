//! The clock `php-fpm -v` carries: php prints `__DATE__ " " __TIME__` of the
//! compile that last rebuilt the host, so this host stamps the Unix
//! seconds into `PHP_BUILD_EPOCH` when this file or its sources change,
//! and `php_sapi::build_date` renders them as php's.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    println!("cargo:rustc-env=PHP_BUILD_EPOCH={seconds}");
}
