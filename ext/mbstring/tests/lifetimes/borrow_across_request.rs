use php_ext::request::Request;
#[derive(Default)]
struct ConversionState(u32);
fn invalid(req: &mut Request) {
    let state = req.state::<ConversionState>();
    req.header_add(b"X-Proof: reentry", true, 0);
    std::hint::black_box(state.0);
}
