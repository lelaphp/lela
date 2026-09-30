//! `php-cgi`, the binary of `main.rs` unchanged under php-cgi's name,
//! which `php_sapi::Host::of` reads off `argv[0]`: php builds its CGI as
//! `php-cgi` beside `php`, and a test that looks for it looks there
//! (`Z/sapi/cgi/tests/include.inc`).
#![forbid(unsafe_code)]

use std::process::ExitCode;

#[path = "main.rs"]
mod host;

fn main() -> ExitCode {
    host::main()
}
