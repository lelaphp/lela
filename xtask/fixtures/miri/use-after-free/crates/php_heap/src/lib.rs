//! A negative control (harness-design.md §4.1, §7's Miri row; #1916): one
//! test reads a box after freeing it, which `xtask miri` must fail as
//! undefined behaviour, and the test beside it must still run, though
//! Miri ends the test binary at the first.

#[cfg(test)]
mod tests {
    #[test]
    fn a_read_after_the_free() {
        let freed = Box::into_raw(Box::new(7_u64));
        // SAFETY: `freed` is the box's own pointer, given back once.
        drop(unsafe { Box::from_raw(freed) });
        // SAFETY: none; the read after the free is the defect this
        // control exists to show.
        let read = unsafe { freed.read() };
        assert_eq!(read, 7);
    }

    #[test]
    fn a_test_beside_it_still_runs() {
        assert_eq!(1 + 1, 2);
    }
}
