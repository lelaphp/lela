//! A test under `tests/` that ends early with `return Ok(())`.

#[test]
fn an_ok_return_before_the_assert() -> Result<(), String> {
    if php_heap::array::text(b"\xff").is_none() {
        return Ok(());
    }
    assert_eq!(php_heap::array::text(b"y"), Some("y"));
    Ok(())
}
