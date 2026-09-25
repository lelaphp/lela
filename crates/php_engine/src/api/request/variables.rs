//! `main/php_variables.c` over the request description: each auto-global
//! laid into `EG(symbol_table)` as `php_hash_environment` lays it at
//! request start, or just in time, where a unit that names it is
//! compiled (`zend_is_auto_global`). The bucket of each
//! name is where the program's `$_SERVER` lives; php keeps the host's
//! arrays a second time as `PG(http_globals)` for `filter_input` and
//! `get_browser`, which read what the host registered through filters of
//! their own and are not built on this table.

use php_unit::AutoGlobal;

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::{Key, PhpArray, PhpString, Value};
use crate::exec::TableId;

impl Request {
    /// `php_hash_environment` (`Z/main/php_variables.c:785`), what
    /// `php_request_startup` runs before the extensions' `RINIT`: `$argv`
    /// and `$argc` written into `EG(symbol_table)` where the host
    /// described an `argv` (`php_build_argv(NULL, NULL)`), then
    /// `zend_activate_auto_globals`, which makes `$_GET`, `$_POST`,
    /// `$_COOKIE` and `$_FILES` now and arms `$_SERVER`, `$_ENV` and
    /// `$_REQUEST` to be made when first named. `$_SERVER` is armed and
    /// not made, as the oracle's order of the table says it is under the
    /// CLI, which names it itself once the request has started and before
    /// it compiles the program ([`Request::arm`]).
    ///
    /// The main op array's frame attaches to the table after this, so its
    /// `$argv` and `$argc` are the table's (`zend_attach_symbol_table`),
    /// and a function's `global $argv` binds the same variable:
    ///
    /// ```php
    /// var_dump($argc, $argv, count($argv) === $argc);
    /// function f() { global $argv; return $argv; }
    /// var_dump(f() === $argv);
    /// $n = '_FILES';
    /// var_dump($$n);
    /// ```
    /// ```expect
    /// int(1)
    /// array(1) {
    ///   [0]=>
    ///   string(%d) "%s"
    /// }
    /// bool(true)
    /// bool(true)
    /// array(0) {
    /// }
    /// ```
    pub(crate) fn hash_environment(&mut self) -> Result<(), Thrown> {
        if !self.description.argv.is_empty() {
            let mut words = PhpArray::new(self)?;
            for word in self.description.argv.clone() {
                let word = PhpString::new(self, &word)?;
                words.append(self, Value::string(word))?;
            }
            let count = i64::try_from(self.description.argv.len()).unwrap_or(i64::MAX);
            self.symbol_update(TableId::GLOBAL, b"argv".to_vec(), Value::array(words));
            self.symbol_update(TableId::GLOBAL, b"argc".to_vec(), Value::int(count));
        }
        self.armed = vec![AutoGlobal::Server, AutoGlobal::Env, AutoGlobal::Request];
        for global in [
            AutoGlobal::Get,
            AutoGlobal::Post,
            AutoGlobal::Cookie,
            AutoGlobal::Files,
        ] {
            self.make(global)?;
        }
        Ok(())
    }

    /// `zend_is_auto_global` (`Z/Zend/zend_compile.c:1987`) for one
    /// auto-global: made and laid into `EG(symbol_table)` where it is
    /// still armed, and nothing where it was made already.
    pub(crate) fn arm(&mut self, global: AutoGlobal) -> Result<(), Thrown> {
        let Some(at) = self.armed.iter().position(|armed| *armed == global) else {
            return Ok(());
        };
        self.armed.remove(at);
        self.make(global)
    }

    /// The auto-global's callback (`php_auto_globals_create_*`): the
    /// array built and bound into `EG(symbol_table)` under the name
    /// (`zend_hash_update`), the table its one owner.
    ///
    /// `$_GET`, `$_POST` and `$_COOKIE` are empty: no host this engine
    /// has yet describes a query string, a form body or a cookie header,
    /// so `treat_data` has nothing to parse, which is what php's CLI
    /// hands it. `$_FILES` is empty until an upload is parsed, as php's
    /// is. `$_REQUEST` merges those three by `request_order`, or
    /// `variables_order` where that is empty, and is empty with them.
    fn make(&mut self, global: AutoGlobal) -> Result<(), Thrown> {
        let order = self
            .ini_get(b"variables_order")
            .map(<[u8]>::to_ascii_uppercase)
            .unwrap_or_default();
        let array = match global {
            AutoGlobal::Server if order.contains(&b'S') => self.create_server()?,
            AutoGlobal::Env if order.contains(&b'E') => self.environment()?,
            _ => PhpArray::new(self)?,
        };
        self.symbol_update(TableId::GLOBAL, global.name().to_vec(), Value::array(array));
        Ok(())
    }

    /// `php_auto_globals_create_server` (`Z/main/php_variables.c:876`)
    /// under a `variables_order` that names `S`:
    /// `php_register_server_variables`, which is the host's rows
    /// (`sapi_module.register_server_variables`) and then
    /// `REQUEST_TIME_FLOAT` and `REQUEST_TIME`; then, where the host
    /// described an `argv`, the `$argv` and `$argc` of `EG(symbol_table)`,
    /// the same array with one more owner.
    ///
    /// The CLI's rows (`sapi_cli_register_variables`,
    /// `Z/sapi/cli/php_cli.c:316`) are the environment first and then
    /// `PHP_SELF`, `SCRIPT_NAME`, `SCRIPT_FILENAME`, `PATH_TRANSLATED` and
    /// `DOCUMENT_ROOT`, which the host describes; every host php has
    /// imports the environment first, so that half is the engine's.
    ///
    /// ```php
    /// $n = '_SERVER';
    /// $s = $$n;
    /// var_dump($s['argc'], $s['argv'] === $argv, $s['PHP_SELF'], $s['SCRIPT_NAME']);
    /// var_dump($s['SCRIPT_FILENAME'] === $s['PATH_TRANSLATED'], $s['DOCUMENT_ROOT']);
    /// var_dump(is_float($s['REQUEST_TIME_FLOAT']), $s['REQUEST_TIME'] === (int) $s['REQUEST_TIME_FLOAT']);
    /// $keys = array_keys($s);
    /// var_dump(array_slice($keys, array_search('PHP_SELF', $keys)));
    /// ```
    /// ```expect
    /// int(1)
    /// bool(true)
    /// string(%d) "%s"
    /// string(%d) "%s"
    /// bool(true)
    /// string(0) ""
    /// bool(true)
    /// bool(true)
    /// array(9) {
    ///   [0]=>
    ///   string(8) "PHP_SELF"
    ///   [1]=>
    ///   string(11) "SCRIPT_NAME"
    ///   [2]=>
    ///   string(15) "SCRIPT_FILENAME"
    ///   [3]=>
    ///   string(15) "PATH_TRANSLATED"
    ///   [4]=>
    ///   string(13) "DOCUMENT_ROOT"
    ///   [5]=>
    ///   string(18) "REQUEST_TIME_FLOAT"
    ///   [6]=>
    ///   string(12) "REQUEST_TIME"
    ///   [7]=>
    ///   string(4) "argv"
    ///   [8]=>
    ///   string(4) "argc"
    /// }
    /// ```
    fn create_server(&mut self) -> Result<PhpArray, Thrown> {
        let mut server = self.environment()?;
        for (name, value) in self.description.server.clone() {
            let value = PhpString::new(self, &value)?;
            let name = PhpString::new(self, &name)?;
            server.set(self, Key::from_string(name), Value::string(value))?;
        }
        // `sapi_get_request_time`: the host keeps no time of its own, so
        // it is the moment the table is made, as `php_time()` makes it.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64());
        let name = PhpString::new(self, b"REQUEST_TIME_FLOAT")?;
        server.set(self, Key::Str(name), Value::float(now))?;
        let name = PhpString::new(self, b"REQUEST_TIME")?;
        #[allow(clippy::cast_possible_truncation)]
        let whole = now as i64;
        server.set(self, Key::Str(name), Value::int(whole))?;
        // `zend_hash_find_ex_ind` of both, and neither where one is
        // missing: the zval as the table holds it, a reference kept one.
        let found = |req: &mut Request, name: &[u8]| {
            req.symbol_value(TableId::GLOBAL, name)
                .filter(|value| value.tag() != crate::api::values::TypeTag::Undef)
        };
        if !self.description.argv.is_empty()
            && let (Some(count), Some(words)) = (found(self, b"argc"), found(self, b"argv"))
        {
            for (name, value) in [(b"argv".as_slice(), words), (b"argc", count)] {
                self.addref(&value);
                let name = PhpString::new(self, name)?;
                server.set(self, Key::Str(name), value)?;
            }
        }
        Ok(server)
    }

    /// `php_import_environment_variables`
    /// (`Z/main/php_variables.c:619`): each variable of the environment
    /// whose name is not empty and holds no space, dot or `[`, under a
    /// name that reads as an integer as that integer, in the order the
    /// environment has them. What `$_ENV` is under a `variables_order`
    /// that names `E`, and the first rows of `$_SERVER`.
    fn environment(&mut self) -> Result<PhpArray, Thrown> {
        let mut array = PhpArray::new(self)?;
        for (name, value) in self.description.env.clone() {
            if name.is_empty() || name.iter().any(|b| matches!(b, b' ' | b'.' | b'[')) {
                continue;
            }
            let value = PhpString::new(self, &value)?;
            let name = PhpString::new(self, &name)?;
            array.set(self, Key::from_string(name), Value::string(value))?;
        }
        Ok(array)
    }
}
