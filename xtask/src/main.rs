#![forbid(unsafe_code)]

fn main() {
    if let Err(err) = xtask::run() {
        eprintln!("xtask: {err:#}");
        // A worktree laid down but not ready is neither done nor undone:
        // 3, so a caller can tell it from a refusal that changed nothing.
        let code = if err.downcast_ref::<xtask::worktree::NotReady>().is_some() {
            3
        } else {
            1
        };
        std::process::exit(code);
    }
}
