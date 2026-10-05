//! `api::Pool`'s drain, what an extension's engine-end function empties
//! its pools with and the engine its own persistent sockets with
//! (ADR-0010's amendment of ADR-0005).

use crate::api::abi::Pool;

/// Every idle entry comes out with its key, two under one key both, and
/// the pool holds none after.
#[test]
fn a_drain_takes_every_idle_entry_out_with_its_key() {
    let pool: Pool<&str, u32> = Pool::new(8);
    assert_eq!(pool.give_back("a", 1), None);
    assert_eq!(pool.give_back("a", 2), None);
    assert_eq!(pool.give_back("b", 3), None);
    let mut drained = pool.drain();
    drained.sort_unstable();
    assert_eq!(drained, [("a", 1), ("a", 2), ("b", 3)]);
    assert_eq!(pool.idle_len(), 0);
    assert_eq!(pool.take(&"a"), None);
}
