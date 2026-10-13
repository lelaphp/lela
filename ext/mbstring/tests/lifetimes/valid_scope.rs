use php_ext::request::Request;
use php_ext::values::FreshString;
use php_ext::raising::Thrown;
#[derive(Default)]
struct ConversionState(u32);
fn valid(req: &mut Request, out: FreshString) -> Result<(), Thrown> {
    std::hint::black_box(out.bytes().len());
    let grown = out.extend(req, 4096)?;
    grown.free(req);
    let state = req.state::<ConversionState>().0;
    req.header_add(b"X-Proof: ended borrow", true, 0);
    std::hint::black_box(state);
    Ok(())
}
