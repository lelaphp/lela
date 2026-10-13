// Negative lifetime control for #2832: rustc must reject E0505. No executable.
use php_ext::request::Request;
use php_ext::values::Emalloc;

fn invalid(req: &mut Request, allocation: Emalloc) -> usize {
    let view = allocation.bytes();
    allocation.free(req);
    view.len()
}

fn main() {}
