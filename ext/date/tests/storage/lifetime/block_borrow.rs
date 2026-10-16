use php_ext::request::Request;
use php_ext::values::Emalloc;

// E0505: freeing while the mutable byte view is subsequently used is rejected.
pub fn bad(req: &mut Request, mut block: Emalloc) {
    let view = block.bytes_mut();
    block.free(req);
    view[0] = 1;
}
