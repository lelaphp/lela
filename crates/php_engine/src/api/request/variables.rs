//! `main/php_variables.c` over the request description: each auto-global
//! laid into `EG(symbol_table)` as `php_hash_environment` lays it at
//! request start, or just in time, where a unit that names it is
//! compiled (`zend_is_auto_global`). The bucket of each
//! name is where the program's `$_SERVER` lives; the same array is kept
//! a second time in `PG(http_globals)` ([`HttpGlobals`]), which is what
//! an extension reads ([`Request::superglobal`]). `filter_input` reads
//! neither: it keeps what the host registered through its filter.

use php_unit::AutoGlobal;

use super::{Request, Symbol};
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

    /// A variable of `EG(symbol_table)` read, `zend_hash_find` there
    /// with an attached name followed to its frame's variable: what the
    /// program's `$GLOBALS[$name]` is now, a view, no owner taken.
    /// `phpinfo()` and the URL rewriter read `$_SERVER` so, and see the
    /// program's writes to it, where [`Request::superglobal`] does not.
    pub fn global(&mut self, name: &[u8]) -> Option<Value> {
        let table = self.global_table();
        self.symbol_value(table, name)
    }

    /// A variable of `EG(symbol_table)` written or deleted, what an
    /// extension that binds a global does: `Some` is
    /// `zend_hash_update_ind`, `value` the table's from here, into the CV
    /// a frame attached to the name where one is, and into the bucket
    /// otherwise, a name the table has not added to it; `None` is
    /// `zend_delete_global_variable`, `zend_hash_del_ind`. What stood
    /// there is released after the write, where a destructor is served.
    /// The session binds `$_SESSION` so (`php_session_track_init`).
    pub fn set_global(&mut self, name: &[u8], value: Option<Value>) {
        let table = self.global_table();
        let Some(value) = value else {
            self.symbol_unset(table, name);
            return;
        };
        let attached = self.table_mut(table).and_then(|entries| {
            entries
                .iter()
                .find(|(key, _)| key == name)
                .and_then(|(_, symbol)| match symbol {
                    Symbol::Indirect(frame, at) => Some((*frame, *at)),
                    Symbol::Value(_) | Symbol::Held(_) => None,
                })
        });
        match attached {
            Some((frame, at)) => {
                let previous = self.set_attached(table, frame, at, value);
                self.release(previous);
            }
            None => self.symbol_update(table, name.to_vec(), value),
        }
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

    /// php's reader of an auto-global, `zend_is_auto_global` and then
    /// `PG(http_globals)[TRACK_VARS_*]`, as `php_zlib_output_encoding`
    /// and `get_browser` read `$_SERVER`: one armed to be made just in
    /// time is made first, and the answer is the array the host made,
    /// the one the program sees as `$_SERVER` until the program writes
    /// to its own. `None` for a name that is no auto-global and for
    /// `$_REQUEST` and `$_SESSION`, which php keeps no track of, and
    /// for one whose making met the memory limit, which the program
    /// meets again at its next allocation.
    ///
    /// `filter_input_array(INPUT_ENV)` reads `$_ENV` so, which a
    /// program that never names it has not made: the read makes it.
    ///
    /// ```php
    /// var_dump(is_array(filter_input_array(INPUT_ENV)));
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    ///
    /// The program's write separates its array from the track, and the
    /// extension reads what the host described, `$_SERVER` as
    /// `zlib_get_coding_type()` reads it and `$_ENV` alike:
    ///
    /// ```php
    /// $_SERVER['HTTP_ACCEPT_ENCODING'] = 'gzip';
    /// ini_set('zlib.output_compression', 'On');
    /// $coding = zlib_get_coding_type();
    /// ini_set('zlib.output_compression', 'Off');
    /// $_ENV['WITNESS'] = 'written';
    /// var_dump($coding, filter_has_var(INPUT_ENV, 'WITNESS'), $_ENV['WITNESS']);
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(7) "written"
    /// ```
    pub fn superglobal(&mut self, name: &[u8]) -> Option<PhpArray> {
        let global = AutoGlobal::of(name)?;
        let _ = self.arm(global);
        self.http_globals.track(global).and_then(|track| *track)
    }

    /// Step 8 of `php_request_shutdown` (`Z/main/main.c:2011`): each
    /// track of `PG(http_globals)` given up, `TRACK_VARS_POST` first,
    /// after the shutdown functions are freed and before
    /// `zend_deactivate` destroys `EG(symbol_table)`.
    pub(crate) fn destroy_http_globals(&mut self) {
        for global in HttpGlobals::TRACKED {
            if let Some(array) = self.http_globals.track(global).and_then(Option::take) {
                self.release(Value::array(array));
            }
        }
    }

    /// The auto-global's callback (`php_auto_globals_create_*`): the
    /// array built and bound into `EG(symbol_table)` under the name
    /// (`zend_hash_update`), and kept a second time in its track of
    /// `PG(http_globals)` where it has one (`Z_ADDREF`), the same array
    /// with two owners, so a write to it from PHP code separates the
    /// program's and leaves the track's as the host made it.
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
        let value = Value::array(array);
        if let Some(track) = self.http_globals.track(global) {
            let kept = track.replace(array);
            self.addref(&value);
            if let Some(kept) = kept {
                self.release(Value::array(kept));
            }
        }
        self.symbol_update(TableId::GLOBAL, global.name().to_vec(), value);
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
    /// (`Z/main/php_variables.c:629`): each variable of the environment
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

/// php's `PG(http_globals)`: the array each auto-global with a
/// `TRACK_VARS_*` of its own was made as, one owner of it beside
/// `EG(symbol_table)`'s, from its making to step 8 of
/// `php_request_shutdown`. `$_REQUEST` and `$_SESSION` have no track.
#[derive(Default)]
pub(crate) struct HttpGlobals {
    post: Option<PhpArray>,
    get: Option<PhpArray>,
    cookie: Option<PhpArray>,
    server: Option<PhpArray>,
    env: Option<PhpArray>,
    files: Option<PhpArray>,
}

impl HttpGlobals {
    /// Each tracked auto-global, in `TRACK_VARS_*` order
    /// (`Z/main/php_globals.h:40`).
    const TRACKED: [AutoGlobal; 6] = [
        AutoGlobal::Post,
        AutoGlobal::Get,
        AutoGlobal::Cookie,
        AutoGlobal::Server,
        AutoGlobal::Env,
        AutoGlobal::Files,
    ];

    /// The track an auto-global's array is kept in, `None` for one
    /// php keeps none of.
    fn track(&mut self, global: AutoGlobal) -> Option<&mut Option<PhpArray>> {
        match global {
            AutoGlobal::Post => Some(&mut self.post),
            AutoGlobal::Get => Some(&mut self.get),
            AutoGlobal::Cookie => Some(&mut self.cookie),
            AutoGlobal::Server => Some(&mut self.server),
            AutoGlobal::Env => Some(&mut self.env),
            AutoGlobal::Files => Some(&mut self.files),
            AutoGlobal::Request | AutoGlobal::Session => None,
        }
    }
}
