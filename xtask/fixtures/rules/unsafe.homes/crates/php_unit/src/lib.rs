//! Not a home: the block below is the fifth place and must fail the audit.
#![forbid(unsafe_code)]
pub fn read(p: *const u8) -> u8 {
    unsafe { *p }
}
