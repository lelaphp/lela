//! `api::Pool`'s drain, what an extension's engine-end function empties
//! its pools with and the engine its own persistent sockets with
//! (ADR-0010's amendment of ADR-0005), and `api::PoolKey`, what a pool
//! keeps a connection named by a credential under.

use crate::api::abi::{Pool, PoolKey};

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

/// A key built from credentials tells apart parts that differ only in
/// where one ends and the next begins, the same bytes split at another
/// colon among them, and two passwords that differ in one byte; the same
/// parts give the same key.
#[test]
fn a_pool_key_tells_apart_parts_split_otherwise_and_one_byte_apart() {
    let key = |parts: &[&[u8]]| PoolKey::of(parts);
    let dsn: &[u8] = b"sqlite::memory:";
    assert_eq!(key(&[dsn, b"a:b", b"c"]), key(&[dsn, b"a:b", b"c"]));
    assert_ne!(key(&[dsn, b"a:b", b"c"]), key(&[dsn, b"a", b"b:c"]));
    assert_ne!(key(&[b"roottest", b""]), key(&[b"root", b"test"]));
    assert_ne!(key(&[dsn, b"u", b"secret"]), key(&[dsn, b"u", b"secreT"]));
    assert_ne!(key(&[dsn, b"u", b""]), key(&[dsn, b"u"]));
}

/// The key is the digest alone, 32 bytes whatever the password's
/// length, so what a pool or a request keeps under it holds no byte of
/// the password in the clear.
#[test]
fn a_pool_key_holds_the_digest_and_no_part() {
    let password = b"correct horse battery staple, long enough to show".as_slice();
    let key = PoolKey::of(&[b"mysql:host=127.0.0.1", b"root", password]);
    assert_eq!(std::mem::size_of::<PoolKey>(), 32);
    let shown = format!("{key:?}");
    assert_eq!(shown.len(), 64);
    assert!(shown.bytes().all(|b| b.is_ascii_hexdigit()));
}
