#![forbid(unsafe_code)]

fn main() {
    if let Err(err) = xtask::run() {
        eprintln!("xtask: {err:#}");
        std::process::exit(1);
    }
}
