//! `xmlreader_open_fn` and `xmlreader_xml_fn`: the copies of the static
//! `open` and `XML` php makes at `MINIT` with `ZEND_ACC_STATIC` taken off,
//! in no function table, which `xmlreader_get_method` answers for a call
//! on an object so the load lands in that object.

use php_ext::abi::{Path, This, Untyped};
use php_ext::attr::php_class;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString, Value};

use crate::{Failure, Target, from_string, from_uri};

/// `XMLReader` as the trampolines are laid on it.
///
/// ```php
/// $r = new XMLReader; var_dump($r->XML('<a>t</a>'), $r->read(), $r->name);
/// $f = sys_get_temp_dir() . '/xmlreader_open.xml'; file_put_contents($f, '<b/>');
/// var_dump($r->open($f), $r->read(), $r->name); unlink($f);
/// var_dump(@$r->open('/nonexistent/x.xml'));
/// try { $r->XML(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(1) "a"
/// bool(true)
/// bool(true)
/// string(1) "b"
/// bool(false)
/// XMLReader::XML(): Argument #1 ($source) must not be empty
/// ```
#[php_class(extern)]
pub(crate) struct XMLReader;

#[php_class(trampolines)]
impl XMLReader {
    fn open(
        req: &mut Request,
        this: This<Self>,
        uri: Path,
        #[php(default = null)] encoding: Option<Path>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let into = Target::This(this.object());
        from_uri(req, into, Failure::Warn, uri, encoding.as_ref(), flags).map(Untyped)
    }

    fn XML(
        req: &mut Request,
        this: This<Self>,
        source: PhpString,
        #[php(default = null)] encoding: Option<Path>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let into = Target::This(this.object());
        from_string(req, into, Failure::Warn, source, encoding.as_ref(), flags).map(Untyped)
    }
}
