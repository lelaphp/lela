use php_ext::request::Request;
use php_ext::values::FreshString;
use php_ext::raising::Thrown;
fn invalid(req: &mut Request, out: FreshString) -> Result<(), Thrown> {
    let view = out.bytes();
    let _grown = out.extend(req, 4096)?;
    std::hint::black_box(view);
    Ok(())
}
