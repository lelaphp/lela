use php_ext::request::Request;
use php_ext::values::FreshString;
fn invalid(req: &mut Request, out: FreshString) {
    let view = out.bytes();
    out.free(req);
    std::hint::black_box(view);
}
