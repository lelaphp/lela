//! `engine_user.c`: an engine declared in PHP, a class implementing
//! `Random\Engine`, drawn from by calling its `generate()`. php keeps the
//! object and its `generate` method in `php_random_status_state_user`;
//! here the state is the object and the request it is served through,
//! for as long as one draw needs them.

use php_ext::call::call_method;
use php_ext::raising::throw_error;
use php_ext::request::Request;
use php_ext::values::PhpObject;

use crate::throwables::Throwables;
use crate::{Algo, Failure, Generated};

/// `php_random_status_state_user`: the engine object, with the request
/// its `generate()` is served on.
pub(crate) struct User<'r> {
    req: &'r mut Request,
    object: PhpObject,
}

impl<'r> User<'r> {
    pub(crate) fn new(req: &'r mut Request, object: PhpObject) -> User<'r> {
        User { req, object }
    }
}

impl Algo for User<'_> {
    /// `generate()` called on the object: its string's first eight bytes
    /// at most, least significant first, and an empty string php's
    /// `BrokenRandomEngineError`.
    ///
    /// ```php
    /// class E implements Random\Engine { public $n = 0; function generate(): string { return $this->n++ ? "\x01\x02" : ""; } }
    /// $r = new Random\Randomizer(new E);
    /// try { $r->nextInt(); } catch (Random\BrokenRandomEngineError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($r->getInt(0, 0xFFFF), $r->nextInt(), bin2hex($r->getBytes(5)));
    /// class Nul implements Random\Engine { function generate(): string { return "\0"; } }
    /// try { (new Random\Randomizer(new Nul))->getInt(0, 100); } catch (Random\BrokenRandomEngineError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// A random engine must return a non-empty string
    /// int(513)
    /// int(256)
    /// string(10) "0102010201"
    /// ```
    fn generate(&mut self) -> Result<Generated, Failure> {
        let answered =
            call_method(self.req, &self.object, b"generate", &[]).map_err(Failure::Thrown)?;
        let value = answered.deref();
        let bytes = value.as_str_bytes().unwrap_or_default();
        let size = bytes.len().min(8);
        let result = bytes
            .iter()
            .take(size)
            .enumerate()
            .fold(0_u64, |acc, (i, b)| acc | (u64::from(*b) << (8 * i)));
        answered.give_up(self.req);
        if size == 0 {
            let thrown = match Throwables::of(self.req) {
                Ok(ce) => throw_error(
                    self.req,
                    ce.broken_random_engine_error,
                    "A random engine must return a non-empty string",
                ),
                Err(fatal) => fatal,
            };
            return Err(Failure::Thrown(thrown));
        }
        Ok(Generated { result, size })
    }
}
