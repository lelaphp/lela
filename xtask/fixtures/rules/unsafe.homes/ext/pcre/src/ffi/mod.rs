//! A path home: `ext/*/src/ffi/`. Two sites: the block, and the unsafe
//! attribute, which asserts a contract as a block does (#46).
pub fn read(p: *const u8) -> u8 {
    // SAFETY: the fixture never runs.
    unsafe { *p }
}

#[unsafe(no_mangle)]
pub extern "C" fn exported() {}
