//! A binary with no denial of its own.

fn main() {
    std::process::exit(i32::from(php_syntax::one()) - 1);
}
