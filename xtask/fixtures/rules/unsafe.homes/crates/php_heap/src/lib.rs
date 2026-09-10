//! A home: `unsafe` is allowed here. Three sites: the block, a block whose
//! brace stands on the line after the keyword, which the audit sees only
//! while it reads the file whole, and the unsafe attribute (#46).
pub fn read(p: *const u8) -> u8 {
    // SAFETY: the fixture never runs.
    unsafe { *p }
}

pub fn split(p: *const u8) -> u8 {
    // SAFETY: the fixture never runs.
    unsafe
    { *p }
}

#[unsafe(no_mangle)]
pub extern "C" fn exported() {}
