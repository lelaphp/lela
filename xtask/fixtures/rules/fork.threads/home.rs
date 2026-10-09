//! The home: the one call the rule allows, under its expectation, which
//! clippy would itself refuse if the lint stopped firing; and a test
//! module's own threads under its `allow`.
pub fn counted() -> bool {
    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture's home starts the one counted thread"
    )]
    let started = std::thread::Builder::new().spawn(|| {});
    started.is_ok()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "a test starts its own threads")]
mod tests {
    #[test]
    fn spawned() {
        std::thread::spawn(|| {}).join().ok();
    }
}
