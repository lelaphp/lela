//! `php-cgi`, the binary (design §4.1; ADR-0008): php's CGI host,
//! [`php_sapi_cgi::main`], built beside `php` as php builds its CGI, where
//! a test that looks for it looks (`Z/sapi/cgi/tests/include.inc`).
#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    php_sapi_cgi::main()
}
