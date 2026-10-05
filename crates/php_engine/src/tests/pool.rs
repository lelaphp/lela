//! `api::Pool`'s drain, what an extension's engine-end function empties
//! its pools with and the engine its own persistent sockets with
//! (ADR-0010's amendment of ADR-0005).

use crate::api::abi::Pool;

/// Every idle entry comes out with its key, the one given back last
/// first, as `zend_hash_graceful_reverse_destroy` destroys
/// `EG(persistent_list)`, two under one key both, and the pool holds none
/// after.
#[test]
fn a_drain_takes_every_idle_entry_out_with_its_key_the_last_given_first() {
    let pool: Pool<&str, u32> = Pool::new(8);
    assert_eq!(pool.give_back("a", 1), None);
    assert_eq!(pool.give_back("b", 2), None);
    assert_eq!(pool.give_back("a", 3), None);
    assert_eq!(pool.give_back("c", 4), None);
    assert_eq!(pool.drain(), [("c", 4), ("a", 3), ("b", 2), ("a", 1)]);
    assert_eq!(pool.idle_len(), 0);
    assert_eq!(pool.take(&"a"), None);
}

/// Past the bound an entry given back is answered to its caller to close,
/// and once the pool is drained every one is, as a request on another
/// thread gives back what it held after the engine's end drained the
/// pool (ADR-0010's amendment of ADR-0005).
#[test]
fn an_entry_past_the_bound_or_after_the_drain_is_answered_to_its_caller() {
    let pool: Pool<&str, u32> = Pool::new(2);
    assert_eq!(pool.give_back("a", 1), None);
    assert_eq!(pool.give_back("a", 2), None);
    assert_eq!(pool.give_back("b", 3), Some(3));
    assert_eq!(pool.take(&"a"), Some(2));
    assert_eq!(pool.drain(), [("a", 1)]);
    assert_eq!(pool.give_back("a", 4), Some(4));
    assert_eq!(pool.idle_len(), 0);
}
