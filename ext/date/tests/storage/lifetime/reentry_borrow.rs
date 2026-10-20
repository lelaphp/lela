use php_ext::request::Request;
use php_ext::values::{Emalloc, PhpObject};

// E0502: a payload borrow cannot remain live across an effect taking Request.
pub fn bad(req: &mut Request, object: PhpObject) {
    let storage = object.native::<Emalloc>(req);
    req.response_mut().write(b"effect");
    if let Some(storage) = storage {
        let _still_used = storage.bytes();
    }
}
