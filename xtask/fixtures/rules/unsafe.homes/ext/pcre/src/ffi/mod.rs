//! A path home: `ext/*/src/ffi/`.
pub fn read(p: *const u8) -> u8 {
    // SAFETY: the fixture never runs.
    unsafe { *p }
}
