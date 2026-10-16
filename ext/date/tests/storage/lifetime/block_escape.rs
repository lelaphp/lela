use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;

// Compilation succeeds; byte storage alone does not prove request lifetime.
pub fn escape(req: &mut Request) -> Result<Emalloc, Thrown> {
    Emalloc::new(req, 104)
}
