//! `main/php_variables.c` over the request description: each auto-global
//! laid into `EG(symbol_table)` as `php_hash_environment` lays it at
//! request start, or just in time, where a unit that names it is
//! compiled (`zend_is_auto_global`). The bucket of each
//! name is where the program's `$_SERVER` lives; the same array is kept
//! a second time in `PG(http_globals)` ([`HttpGlobals`]), which is what
//! an extension reads ([`Request::superglobal`]).
//!
//! php's variable registration is here and nowhere else: the query
//! string, the cookie header and `parse_str()`'s string split by
//! `php_default_treat_data`, a form body by `php_std_post_handler`'s
//! `add_post_var`, each name walked into nested arrays by
//! `php_register_variable_ex`, and every variable a host describes handed
//! first to the input filter an extension registered, as `ext/filter`
//! registers `php_sapi_filter` ([`Input`]). What reads the body is the
//! body's own file, `post`.

use php_unit::AutoGlobal;

use super::{Request, Symbol};
use crate::api::raising::{Level, Thrown, docref};
use crate::api::values::{Key, PhpArray, PhpString, Value};
use crate::exec::TableId;

/// Where a variable php registers came from, php's `PARSE_*` as
/// `sapi_module.input_filter` is handed it (`Z/main/php_variables.h:25`):
/// what an input filter an extension registers is asked about
/// ([`crate::api::abi::ExtensionBuilder::input_filter`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// `PARSE_POST`, a form body's.
    Post,
    /// `PARSE_GET`, the query string's.
    Get,
    /// `PARSE_COOKIE`, the cookie header's.
    Cookie,
    /// `PARSE_SERVER`, a row the host registers into `$_SERVER`.
    Server,
    /// `PARSE_ENV`, the environment's.
    Env,
    /// `PARSE_STRING`, `parse_str()`'s.
    String,
}

/// The limits a request's variables are read under: the ini entries, or
/// what `request_parse_body()` was handed in their place
/// (`REQUEST_PARSE_BODY_OPTION_GET`, `Z/main/SAPI.h:130`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BodyLimits {
    /// `max_file_uploads`.
    pub max_file_uploads: Option<i64>,
    /// `max_input_vars`.
    pub max_input_vars: Option<i64>,
    /// `max_multipart_body_parts`.
    pub max_multipart_body_parts: Option<i64>,
    /// `post_max_size`.
    pub post_max_size: Option<i64>,
    /// `upload_max_filesize`.
    pub upload_max_filesize: Option<i64>,
}

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
        // `php_startup_auto_globals` (`Z/main/php_variables.c:973`): with
        // `auto_globals_jit`, `$_ENV` and `$_REQUEST` are made when named,
        // and `$_SERVER` too while `register_argc_argv` is off, since it
        // is read at module startup, before any host has handed an
        // `argc`; every other one is made now, in the order registered.
        let jit = self.ini_flag(b"auto_globals_jit");
        let server_jit = jit && !self.ini_flag(b"register_argc_argv");
        self.armed = [
            (AutoGlobal::Server, server_jit),
            (AutoGlobal::Env, jit),
            (AutoGlobal::Request, jit),
        ]
        .into_iter()
        .filter_map(|(global, armed)| armed.then_some(global))
        .collect();
        for global in [
            AutoGlobal::Get,
            AutoGlobal::Post,
            AutoGlobal::Cookie,
            AutoGlobal::Server,
            AutoGlobal::Env,
            AutoGlobal::Request,
            AutoGlobal::Files,
        ] {
            if !self.armed.contains(&global) {
                self.make(global)?;
            }
        }
        Ok(())
    }

    /// An ini entry read as `OnUpdateBool` reads it.
    pub(crate) fn ini_flag(&self, name: &[u8]) -> bool {
        self.ini_get(name).is_some_and(|value| {
            value.eq_ignore_ascii_case(b"on")
                || value.eq_ignore_ascii_case(b"yes")
                || value.eq_ignore_ascii_case(b"true")
                || atol(value) != 0
        })
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

    /// The auto-global's callback (`php_auto_globals_create_*`,
    /// `Z/main/php_variables.c:803-960`): the array built and bound into
    /// `EG(symbol_table)` under the name (`zend_hash_update`), and kept a
    /// second time in its track of `PG(http_globals)` where it has one
    /// (`Z_ADDREF`), the same array with two owners, so a write to it from
    /// PHP code separates the program's and leaves the track's as the
    /// host made it.
    ///
    /// `$_GET` is the query string and `$_COOKIE` the cookie header, each
    /// read by `php_default_treat_data` where `variables_order` names it;
    /// `$_POST` the body, read by the reader its content type chose, where
    /// the order names it, no header has gone and the method is `POST`;
    /// `$_FILES` what that reader uploaded, or empty; `$_REQUEST` the three
    /// merged by `request_order`, or `variables_order` where that is
    /// empty. A host with no request of the web describes none of these,
    /// and they are empty, as php's CLI has them.
    ///
    /// ```php
    /// var_dump($_GET, $_POST, $_COOKIE, $_REQUEST);
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// array(0) {
    /// }
    /// array(0) {
    /// }
    /// array(0) {
    /// }
    /// ```
    fn make(&mut self, global: AutoGlobal) -> Result<(), Thrown> {
        let order = self
            .ini_get(b"variables_order")
            .map(<[u8]>::to_ascii_uppercase)
            .unwrap_or_default();
        let array = match global {
            AutoGlobal::Server if order.contains(&b'S') => self.create_server()?,
            AutoGlobal::Env if order.contains(&b'E') => self.environment()?,
            AutoGlobal::Get if order.contains(&b'G') => {
                let query = self.web().and_then(|web| web.query.clone());
                self.treat_data(Input::Get, query.as_deref())?
            }
            AutoGlobal::Cookie if order.contains(&b'C') => {
                let cookies = self.web().and_then(|web| web.cookies.clone());
                self.treat_data(Input::Cookie, cookies.as_deref())?
            }
            AutoGlobal::Post
                if order.contains(&b'P')
                    && !self.output().headers_sent()
                    && self.web().is_some_and(|web| {
                        web.method
                            .as_ref()
                            .is_some_and(|method| method.name().eq_ignore_ascii_case(b"POST"))
                    }) =>
            {
                let array = PhpArray::new(self)?;
                self.handle_post(array, &BodyLimits::default())?;
                array
            }
            AutoGlobal::Files => match self.http_globals.files {
                Some(files) => {
                    self.symbol_update(
                        TableId::GLOBAL,
                        global.name().to_vec(),
                        Value::array(files),
                    );
                    self.addref(&Value::array(files));
                    return Ok(());
                }
                None => PhpArray::new(self)?,
            },
            AutoGlobal::Request => self.form_variables()?,
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

    /// The request of the web the host described, where it described one.
    pub(crate) fn web(&self) -> Option<&crate::engine::WebRequest> {
        self.description.web.as_ref()
    }

    /// `php_auto_globals_create_request` (`Z/main/php_variables.c:920`):
    /// `$_GET`, `$_POST` and `$_COOKIE` as their tracks hold them, merged
    /// in the order `request_order` names them, or `variables_order`
    /// where that is empty, each once.
    fn form_variables(&mut self) -> Result<PhpArray, Thrown> {
        let order = match self.ini_get(b"request_order").filter(|o| !o.is_empty()) {
            Some(order) => order.to_vec(),
            None => self
                .ini_get(b"variables_order")
                .unwrap_or_default()
                .to_vec(),
        };
        let mut form = PhpArray::new(self)?;
        let mut merged = [false; 3];
        for letter in order {
            let (at, global) = match letter.to_ascii_uppercase() {
                b'G' => (0, AutoGlobal::Get),
                b'P' => (1, AutoGlobal::Post),
                b'C' => (2, AutoGlobal::Cookie),
                _ => continue,
            };
            if std::mem::replace(&mut merged[at], true) {
                continue;
            }
            if let Some(source) = self.http_globals.track(global).and_then(|track| *track) {
                form = self.merge(form, source)?;
            }
        }
        Ok(form)
    }

    /// `php_autoglobal_merge` (`Z/main/php_variables.c:762`): each element
    /// of `source` into `dest`, where both hold an array under the key the
    /// two merged the same way, and every other element written over what
    /// `dest` held.
    fn merge(&mut self, mut dest: PhpArray, source: PhpArray) -> Result<PhpArray, Thrown> {
        for (key, value) in source.snapshot() {
            let into = dest.get(&key).and_then(|had| had.as_array().copied());
            if let (Some(nested), Some(into)) = (value.as_array().copied(), into) {
                let into = into.copy(self)?;
                let merged = self.merge(into, nested)?;
                let key = self.owned_key(&key);
                dest.set(self, key, Value::array(merged))?;
            } else {
                self.addref(&value);
                let key = self.owned_key(&key);
                dest.set(self, key, value)?;
            }
        }
        Ok(dest)
    }

    /// A key's second owner, for a write that gives one up.
    fn owned_key(&mut self, key: &Key) -> Key {
        match key {
            Key::Int(n) => Key::Int(*n),
            Key::Str(s) => {
                self.addref(&Value::string(*s));
                Key::Str(*s)
            }
        }
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
        // The host's rows each through the input filter, `PARSE_SERVER`,
        // as `cli_register_variables` and `sapi_cgi_register_variables`
        // hand them, and registered as `php_register_variable_safe` does.
        for (name, value) in self.description.server.clone() {
            if let Some(value) = self.filter_input(Input::Server, &name, &value)? {
                self.register_in(server, &name, value, false)?;
            }
        }
        // `sapi_get_request_time`, when the request started.
        let now = self.started().as_secs_f64();
        let name = PhpString::new(self, b"REQUEST_TIME_FLOAT")?;
        server.set(self, Key::Str(name), Value::float(now))?;
        let name = PhpString::new(self, b"REQUEST_TIME")?;
        let whole = self.request_time();
        server.set(self, Key::Str(name), Value::int(whole))?;
        // `zend_hash_find_ex_ind` of both, and neither where one is
        // missing: the zval as the table holds it, a reference kept one.
        let found = |req: &mut Request, name: &[u8]| {
            req.symbol_value(TableId::GLOBAL, name)
                .filter(|value| value.tag() != crate::api::values::TypeTag::Undef)
        };
        if !self.description.argv.is_empty() {
            if let (Some(count), Some(words)) = (found(self, b"argc"), found(self, b"argv")) {
                for (name, value) in [(b"argv".as_slice(), words), (b"argc", count)] {
                    self.addref(&value);
                    let name = PhpString::new(self, name)?;
                    server.set(self, Key::Str(name), value)?;
                }
            }
        } else if self.ini_flag(b"register_argc_argv") {
            // `php_build_argv(query_string, track)`: the query string cut
            // at each `+`, where no host handed an `argc`.
            self.note_error(
                Level::Deprecated,
                "Deriving $_SERVER['argv'] from the query string is deprecated. Configure register_argc_argv=0 to turn this message off",
            );
            let query = self
                .web()
                .and_then(|web| web.query.clone())
                .unwrap_or_default();
            let query = query.split(|b| *b == 0).next().unwrap_or_default();
            let mut words = PhpArray::new(self)?;
            let mut count = 0;
            if !query.is_empty() {
                for word in query.split(|b| *b == b'+') {
                    let word = PhpString::new(self, word)?;
                    words.append(self, Value::string(word))?;
                    count += 1;
                }
            }
            let name = PhpString::new(self, b"argv")?;
            server.set(self, Key::Str(name), Value::array(words))?;
            let name = PhpString::new(self, b"argc")?;
            server.set(self, Key::Str(name), Value::int(count))?;
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

impl Request {
    /// `parse_str()`'s half that is php's variable registration,
    /// `php_default_treat_data(PARSE_STRING, string, array)`, into a fresh
    /// array: the string up to its first NUL split on `arg_separator.input` as `php_strtok_r`
    /// splits it, each name and value `php_url_decode`d, each variable
    /// handed to the input filter, and each name walked into nested arrays
    /// by its brackets, as `$_GET` is built.
    ///
    /// ```php
    /// parse_str("a.b=1&c[]=2&c[x]=3&%61=4", $o); var_dump($o);
    /// ```
    /// ```expect
    /// array(3) {
    ///   ["a_b"]=>
    ///   string(1) "1"
    ///   ["c"]=>
    ///   array(2) {
    ///     [0]=>
    ///     string(1) "2"
    ///     ["x"]=>
    ///     string(1) "3"
    ///   }
    ///   ["a"]=>
    ///   string(1) "4"
    /// }
    /// ```
    pub fn parse_query(&mut self, string: &[u8]) -> Result<PhpArray, Thrown> {
        let array = PhpArray::new(self)?;
        self.treat_into(Input::String, Some(string), array, &BodyLimits::default())?;
        Ok(array)
    }

    /// `php_register_variable_ex(name, value, array)`
    /// (`Z/main/php_variables.c:108`): the name less its leading spaces,
    /// each space and dot of it before a `[` read as `_`, and each
    /// bracketed index after it one level of nested array, `[]` the next
    /// index; what an input filter keeps of the raw variables it is handed
    /// is registered so, as `php_sapi_filter` keeps its own. `value` is the
    /// array's from here.
    pub fn register_variable(
        &mut self,
        into: PhpArray,
        name: &[u8],
        value: Value,
    ) -> Result<(), Thrown> {
        self.register_in(into, name, value, false)
    }

    /// `php_default_treat_data` (`Z/main/php_variables.c:455`) for the
    /// query string and the cookie header: a fresh array, and each
    /// variable of `source` registered into it.
    pub(crate) fn treat_data(
        &mut self,
        input: Input,
        source: Option<&[u8]>,
    ) -> Result<PhpArray, Thrown> {
        let array = PhpArray::new(self)?;
        self.treat_into(input, source, array, &BodyLimits::default())?;
        Ok(array)
    }

    /// `php_default_treat_data`'s loop over `source`: split on
    /// `arg_separator.input`, or `;` for a cookie header, empty pieces
    /// skipped as `php_strtok_r` skips them; a cookie's name its leading
    /// white space taken off and an empty one passed over; more than
    /// `max_input_vars` the warning and the rest dropped; a value
    /// `php_url_decode`d, a cookie's `php_raw_url_decode`d, and a name
    /// decoded but a cookie's; each handed to the input filter and
    /// registered.
    fn treat_into(
        &mut self,
        input: Input,
        source: Option<&[u8]>,
        into: PhpArray,
        limits: &BodyLimits,
    ) -> Result<(), Thrown> {
        let Some(source) = source.filter(|source| !source.is_empty()) else {
            return Ok(());
        };
        let source = source.split(|b| *b == 0).next().unwrap_or_default();
        let separators = match input {
            Input::Cookie => b";".to_vec(),
            _ => self
                .ini_get(b"arg_separator.input")
                .unwrap_or(b"&")
                .to_vec(),
        };
        let max_input_vars = limits
            .max_input_vars
            .unwrap_or_else(|| self.ini_long(b"max_input_vars"));
        let mut count = 0i64;
        for mut var in source
            .split(|b| separators.contains(b))
            .filter(|piece| !piece.is_empty())
        {
            let eq = var.iter().position(|b| *b == b'=');
            if input == Input::Cookie {
                let skip = var.iter().take_while(|b| is_space(**b)).count();
                if Some(skip) == eq || skip == var.len() {
                    continue;
                }
                var = &var[skip..];
            }
            count += 1;
            if count > max_input_vars {
                docref(
                    self,
                    Level::Warning,
                    &format!(
                        "Input variables exceeded {max_input_vars}. To increase the limit change max_input_vars in php.ini."
                    ),
                );
                break;
            }
            let eq = var.iter().position(|b| *b == b'=');
            let (name, value) = match eq {
                Some(eq) => {
                    let raw = &var[eq + 1..];
                    let value = url_decode(raw, input != Input::Cookie);
                    (&var[..eq], value)
                }
                None => (var, Vec::new()),
            };
            let name = if input == Input::Cookie {
                name.to_vec()
            } else {
                url_decode(name, true)
            };
            // RFC 2965 lists the more specific path first: a cookie of a
            // name `$_COOKIE` already holds is passed over whole, before
            // the filter sees it (`php_sapi_filter`).
            if input == Input::Cookie {
                let key = Key::from_string(PhpString::new(self, &name)?);
                let held = into.get(&key).is_some();
                if let Key::Str(key) = key {
                    self.release(Value::string(key));
                }
                if held {
                    continue;
                }
            }
            if let Some(value) = self.filter_input(input, &name, &value)? {
                self.register_in(into, &name, value, input == Input::Cookie)?;
            }
        }
        Ok(())
    }

    /// `php_std_post_handler`'s `add_post_vars` (`Z/main/php_variables.c:342`)
    /// over the form body: split on `&` alone, every piece counted, an
    /// empty one too, and the warning past `max_input_vars` ending it; a
    /// name and a value each `php_url_decode`d, handed to the input
    /// filter and registered.
    pub(crate) fn add_post_vars(
        &mut self,
        into: PhpArray,
        body: &[u8],
        limits: &BodyLimits,
    ) -> Result<(), Thrown> {
        let max_input_vars = limits
            .max_input_vars
            .unwrap_or_else(|| self.ini_long(b"max_input_vars"));
        let mut rest = body;
        let mut count = 0i64;
        while !rest.is_empty() {
            let end = rest.iter().position(|b| *b == b'&').unwrap_or(rest.len());
            let var = &rest[..end];
            rest = rest.get(end + 1..).unwrap_or_default();
            let (name, value) = match var.iter().position(|b| *b == b'=') {
                Some(eq) => (&var[..eq], url_decode(&var[eq + 1..], true)),
                None => (var, Vec::new()),
            };
            let name = url_decode(name, true);
            if let Some(value) = self.filter_input(Input::Post, &name, &value)? {
                self.register_in(into, &name, value, false)?;
            }
            count += 1;
            if count > max_input_vars {
                docref(
                    self,
                    Level::Warning,
                    &format!(
                        "Input variables exceeded {max_input_vars}. To increase the limit change max_input_vars in php.ini."
                    ),
                );
                break;
            }
        }
        Ok(())
    }

    /// `sapi_module.input_filter` for one variable: the filter the last
    /// extension in bundle order registered, as `sapi_register_input_filter`
    /// keeps the last, or `php_default_input_filter`, which keeps the
    /// value as it came; `None` where the filter takes it out.
    pub(crate) fn filter_input(
        &mut self,
        input: Input,
        name: &[u8],
        value: &[u8],
    ) -> Result<Option<Value>, Thrown> {
        let shared = std::sync::Arc::clone(&self.shared);
        let hook = shared
            .extensions
            .iter()
            .rev()
            .find_map(|extension| extension.input_filter);
        match hook {
            Some(hook) => hook(self, input, name, value),
            None => Ok(Some(Value::string(PhpString::new(self, value)?))),
        }
    }

    /// An ini entry read as `OnUpdateLong` reads it, C's `atol`.
    pub(crate) fn ini_long(&self, name: &[u8]) -> i64 {
        atol(self.ini_get(name).unwrap_or_default())
    }

    /// `php_register_variable_ex` into `track`, with `cookie` saying the
    /// track is `$_COOKIE`'s, whose plain name already there keeps the
    /// first value (RFC 2965's more specific path first).
    pub(crate) fn register_in(
        &mut self,
        track: PhpArray,
        var_name: &[u8],
        value: Value,
        cookie: bool,
    ) -> Result<(), Thrown> {
        // The name is a C string: it ends at its first NUL.
        let var_name = var_name.split(|c| *c == 0).next().unwrap_or(&[]);
        let skip = var_name.iter().take_while(|c| **c == b' ').count();
        let original = &var_name[skip..];
        let mut var = original.to_vec();
        let mut bracket = None;
        for (i, c) in var.iter_mut().enumerate() {
            if *c == b' ' || *c == b'.' {
                *c = b'_';
            } else if *c == b'[' {
                bracket = Some(i);
                break;
            }
        }
        let var_len = bracket.unwrap_or(var.len());
        if var_len == 0 {
            self.release(value);
            return Ok(());
        }
        let max_nesting = self.ini_long(b"max_input_nesting_level");
        let place = Place {
            track,
            original,
            cookie,
        };
        // The walk: `index` is the key the next write lands at, `None`
        // for `[]`; `path` the containers entered so far.
        let mut index: Option<Vec<u8>> = Some(var[..var_len].to_vec());
        let Some(mut ip) = bracket else {
            return self.write_at(&place, &[], index.as_deref(), value);
        };
        let mut path: Vec<Key> = Vec::new();
        let mut nest = 0i64;
        loop {
            nest += 1;
            if nest > max_nesting {
                let top = Key::from_string(PhpString::new(self, &var[..var_len])?);
                let mut top_level = track;
                top_level.unset(self, &top)?;
                if let Key::Str(name) = top {
                    self.release(Value::string(name));
                }
                self.release(value);
                if !self.ini_flag(b"display_errors") {
                    docref(
                        self,
                        Level::Warning,
                        &format!(
                            "Input variable nesting level exceeded {max_nesting}. To increase the limit change max_input_nesting_level in php.ini."
                        ),
                    );
                }
                return Ok(());
            }
            ip += 1;
            let index_s = ip;
            if var.get(ip).is_some_and(|c| is_space(*c)) {
                ip += 1;
            }
            let new_index: Option<Vec<u8>> = if var.get(ip) == Some(&b']') {
                None
            } else if let Some(close) = var[ip.min(var.len())..].iter().position(|c| *c == b']') {
                ip += close;
                Some(var[index_s..ip].to_vec())
            } else {
                // Not an index: the `[` becomes `_`, and so does every ` `,
                // `.` and `[` after it. php writes over its buffer, so the
                // name grows only at the top level; a nested index ends at
                // the NUL its own `]` was overwritten with, and stays as it
                // was.
                let mut name = index.take();
                if path.is_empty() {
                    let grown = name.get_or_insert_with(Vec::new);
                    grown.push(b'_');
                    for &c in &var[index_s..] {
                        grown.push(if matches!(c, b' ' | b'.' | b'[') {
                            b'_'
                        } else {
                            c
                        });
                    }
                }
                return self.write_at(&place, &path, name.as_deref(), value);
            };
            // Enter, or make, the container at `index`.
            let key = match &index {
                None => None,
                Some(index) => {
                    if forbidden(index, original) {
                        self.release(value);
                        return Ok(());
                    }
                    Some(Key::from_string(PhpString::new(self, index)?))
                }
            };
            let Some(key) = self.enter(track, &path, key)? else {
                self.release(value);
                return Ok(());
            };
            path.push(key);
            index = new_index;
            ip += 1;
            if var.get(ip) != Some(&b'[') {
                return self.write_at(&place, &path, index.as_deref(), value);
            }
        }
    }

    /// One step into the walk: the container at `key` (the next index for
    /// `None`), an array made where it is none, and the key it now stands
    /// under; `None` where the next index is taken.
    fn enter(
        &mut self,
        track: PhpArray,
        path: &[Key],
        key: Option<Key>,
    ) -> Result<Option<Key>, Thrown> {
        let mut parent = descend(track, path);
        let Some(key) = key else {
            let fresh = PhpArray::new(self)?;
            if !parent.try_append(self, Value::array(fresh))? {
                return Ok(None);
            }
            let last = parent.snapshot().last().map(|(k, _)| k.clone());
            return Ok(last.map(|key| self.owned_key(&key)));
        };
        if parent.get(&key).is_none_or(|v| v.as_array().is_none()) {
            let fresh = PhpArray::new(self)?;
            let slot = self.owned_key(&key);
            parent.set(self, slot, Value::array(fresh))?;
        }
        Ok(Some(key))
    }

    /// `plain_var:`: the value appended, or written at `index` as a
    /// symtable key, in the container at `path`; a cookie's plain name
    /// already in `$_COOKIE` keeps what it had.
    fn write_at(
        &mut self,
        place: &Place<'_>,
        path: &[Key],
        index: Option<&[u8]>,
        value: Value,
    ) -> Result<(), Thrown> {
        let mut container = descend(place.track, path);
        let Some(index) = index else {
            container.try_append(self, value)?;
            return Ok(());
        };
        if forbidden(index, place.original) {
            self.release(value);
            return Ok(());
        }
        let key = Key::from_string(PhpString::new(self, index)?);
        if place.cookie && path.is_empty() && container.get(&key).is_some() {
            if let Key::Str(name) = key {
                self.release(Value::string(name));
            }
            self.release(value);
            return Ok(());
        }
        container.set(self, key, value)
    }
}

/// Where one registration writes: the track, the name as it came, and
/// whether the track is `$_COOKIE`'s.
struct Place<'a> {
    track: PhpArray,
    original: &'a [u8],
    cookie: bool,
}

/// The array at `path` inside `track`. Every container the walk
/// makes has the one owner that holds it, so a write through this
/// name of it lands in place, where php's `SEPARATE_ARRAY` would find
/// nothing to separate.
fn descend(track: PhpArray, path: &[Key]) -> PhpArray {
    let mut current = track;
    for key in path {
        match current.get(key).and_then(|v| v.as_array().copied()) {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

/// `php_is_forbidden_variable_name`: a name the mangling turned into a
/// `__Host-` or `__Secure-` cookie prefix it did not have.
fn forbidden(mangled: &[u8], original: &[u8]) -> bool {
    [&b"__Host-"[..], b"__Secure-"]
        .iter()
        .any(|prefix| mangled.starts_with(prefix) && !original.starts_with(prefix))
}

/// C's `isspace`.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// C's `atol`: leading white space, a sign, the digits that follow, and
/// zero for none.
pub(crate) fn atol(bytes: &[u8]) -> i64 {
    let bytes = &bytes[bytes.iter().take_while(|b| is_space(**b)).count()..];
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut value: i64 = 0;
    for digit in digits.iter().take_while(|b| b.is_ascii_digit()) {
        value = value.wrapping_mul(10).wrapping_add(i64::from(digit - b'0'));
    }
    if negative {
        value.wrapping_neg()
    } else {
        value
    }
}

/// `php_url_decode` with `plus`, where `+` is a space, and
/// `php_raw_url_decode` without: `%` and two hex digits a byte, anything
/// else as it is.
pub(crate) fn url_decode(s: &[u8], plus: bool) -> Vec<u8> {
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' if plus => out.push(b' '),
            b'%' if i + 2 < s.len() => match (hex(s[i + 1]), hex(s[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    out
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
    pub(crate) files: Option<PhpArray>,
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
