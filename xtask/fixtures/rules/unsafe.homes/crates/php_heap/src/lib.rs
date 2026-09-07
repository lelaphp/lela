//! A home: `unsafe` is allowed here.
pub fn read(p: *const u8) -> u8 {
    // SAFETY: the fixture never runs.
    unsafe { *p }
}
