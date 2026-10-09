//! Scoped threads, joined before the call returns but uncounted while
//! they run, so a fork in another request could copy one.
pub fn scoped() {
    std::thread::scope(|s| {
        s.spawn(|| {});
    });
}
