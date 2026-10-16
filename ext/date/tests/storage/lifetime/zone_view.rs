use php_ext::request::Request;

// E0515 rejects this view, but does not fix the owned Arc escape case.
pub fn bad(req: &mut Request) -> &[u8] {
    match php_ext_date::get_timezone_info(req) {
        Ok(zone) => zone.name(),
        Err(_) => b"",
    }
}
