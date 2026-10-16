use std::sync::Arc;

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext_date::Zone;

// Compilation succeeds: the current surface does not bind this owner to req.
// Do not execute an escaped owner's read or destruction after request teardown.
pub fn escape(req: &mut Request) -> Result<Arc<Zone>, Thrown> {
    php_ext_date::get_timezone_info(req)
}
