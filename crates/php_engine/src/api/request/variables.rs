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

use super::{Overwrite, Request};
use crate::api::raising::{Level, Thrown, docref};
use crate::api::values::{Key, Owner, PhpArray, PhpString, Value, walk};
use crate::c_number::is_space;
use crate::engine::Known;
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

/// Each auto-global not made yet, whose field no file but this one
/// reads.
#[derive(Default)]
pub(super) struct Variables {
    /// Each auto-global made just in time and not made yet, php's
    /// `armed` on each of `CG(auto_globals)`: the first
    /// `zend_is_auto_global` of one lays it into `EG(symbol_table)`
    /// ([`Request::arm`]).
    armed: Vec<php_unit::AutoGlobal>,
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
            // `ZVAL_STRING`, `zend_string_init`, which makes even a word
            // of one byte a string of its own.
            for word in self.description.argv.clone() {
                let word = Owner::adopt(self.string_init(&word)?);
                words.append(self, word.into_value())?;
            }
            let count = i64::try_from(self.description.argv.len()).unwrap_or(i64::MAX);
            let words = words.into_value().hand_on();
            // `ZSTR_KNOWN(ZEND_STR_ARGV)` and `ZEND_STR_ARGC`
            // (`php_build_argv`, `Z/main/php_variables.c:703`).
            let words_name = self.known(Known::Argv)?;
            self.symbol_update(TableId::GLOBAL, words_name, words);
            let count_name = self.known(Known::Argc)?;
            self.symbol_update(TableId::GLOBAL, count_name, Value::int(count));
        }
        // `php_startup_auto_globals` (`Z/main/php_variables.c:973`): with
        // `auto_globals_jit`, `$_ENV` and `$_REQUEST` are made when named,
        // and `$_SERVER` too while `register_argc_argv` is off, since it
        // is read at module startup, before any host has handed an
        // `argc`; every other one is made now, in the order registered.
        let jit = self.ini_flag(b"auto_globals_jit");
        let server_jit = jit && !self.ini_flag(b"register_argc_argv");
        self.variables.armed = [
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
            if !self.variables.armed.contains(&global) {
                self.make(global)?;
            }
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

    /// The names `EG(symbol_table)` holds, in its order, what the
    /// shell's completion of a `$` walks
    /// (`cli_completion_generator_var`).
    pub fn global_names(&mut self) -> Vec<Vec<u8>> {
        let table = self.global_table();
        self.table_mut(table)
            .map(|entries| {
                entries
                    .iter()
                    .map(|(name, _)| name.as_bytes().to_vec())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A variable of `EG(symbol_table)` written or deleted, what an
    /// extension that binds a global does: `Some` is
    /// `zend_hash_update_ind`, a sink whose owner the table takes, into
    /// the CV a frame attached to the name where one is, and into the
    /// bucket otherwise, a name the table has not added to it; `None` is
    /// `zend_delete_global_variable`, `zend_hash_del_ind`. What stood
    /// there is released after the write, where a destructor is served.
    /// The session binds `$_SESSION` so (`php_session_track_init`), and
    /// the CLI's `-R` binds `$argn` and `$argi`. The name is keyed as
    /// `ZSTR_INIT_LITERAL` and `zend_hash_str_update` key it,
    /// `zend_string_init`, a string of the request's that the table keeps
    /// where it adds the name and that goes where the name stood already.
    pub fn set_global(&mut self, name: &[u8], value: Option<Owner<Value>>) {
        let table = self.global_table();
        match value {
            Some(value) => match self.string_init(name) {
                Ok(key) => {
                    let key = Owner::adopt(key);
                    self.symbol_update_ind(table, *key, value.hand_on(), Overwrite::InPlace);
                    key.give_up(self);
                }
                Err(_) => value.give_up(self),
            },
            None => self.symbol_unset(table, name),
        }
    }

    /// `php_build_argv(NULL, PG(http_globals)[TRACK_VARS_SERVER])` over
    /// `argv`, `SG(request_info).argv` from here, as phpdbg's `run` with
    /// arguments rebuilds them in the request standing
    /// (`Z/main/php_variables.c:658-713`): one array, `$argv` and `$argc`
    /// written into `EG(symbol_table)` through the CV a frame attached to
    /// either, and into `$_SERVER` where it was made; a `$_SERVER` made
    /// later reads them from the table.
    pub(crate) fn build_argv(&mut self, argv: Vec<Vec<u8>>) -> Result<(), Thrown> {
        self.description.argv = argv;
        let server = self.http_globals.server.is_some();
        if self.description.argv.is_empty() && !server {
            return Ok(());
        }
        let mut words = PhpArray::new(self)?;
        for word in self.description.argv.clone() {
            let word = PhpString::new(self, &word)?;
            words.append(self, word.into_value())?;
        }
        let words = words.into_value().hand_on();
        let count = Value::int(i64::try_from(self.description.argv.len()).unwrap_or(i64::MAX));
        if !self.description.argv.is_empty() {
            // `zend_hash_update(&EG(symbol_table), ZSTR_KNOWN(ZEND_STR_ARGV))`
            // and `ZEND_STR_ARGC`'s, through the CV a frame attached.
            let table = self.global_table();
            let words_name = self.known(Known::Argv)?;
            let list = Owner::take(self, words);
            self.symbol_update_ind(table, words_name, list.hand_on(), Overwrite::InPlace);
            let count_name = self.known(Known::Argc)?;
            self.symbol_update_ind(table, count_name, count, Overwrite::InPlace);
        }
        if server {
            let held = Owner::take(self, words);
            self.update_server(b"argv", held)?;
            let held = Owner::take(self, count);
            self.update_server(b"argc", held)?;
        }
        self.release(words);
        Ok(())
    }

    /// `zend_is_auto_global` (`Z/Zend/zend_compile.c:1987`) for one
    /// auto-global: made and laid into `EG(symbol_table)` where it is
    /// still armed, and nothing where it was made already.
    pub(crate) fn arm(&mut self, global: AutoGlobal) -> Result<(), Thrown> {
        let Some(at) = self
            .variables
            .armed
            .iter()
            .position(|armed| *armed == global)
        else {
            return Ok(());
        };
        self.variables.armed.remove(at);
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
        self.arm(global).ok();
        self.http_globals.track(global).and_then(|track| *track)
    }

    /// `PG(http_globals)` for an auto-global as it stands, made or not,
    /// and never made by the asking: what `Phar::webPhar` reads `$_SERVER`
    /// through, whose `Z_TYPE(...) != IS_UNDEF` asks whether the program
    /// has named it. A view: no owner taken.
    #[must_use]
    pub fn made_superglobal(&mut self, name: &[u8]) -> Option<PhpArray> {
        let global = AutoGlobal::of(name)?;
        self.http_globals.track(global).and_then(|track| *track)
    }

    /// `zend_hash_str_update` on `PG(http_globals)[TRACK_VARS_SERVER]` in
    /// place, which php allows `$_SERVER` alone (`HT_ALLOW_COW_VIOLATION`,
    /// `Z/main/php_variables.c:902`): the program's `$_SERVER`, the same
    /// array while it has not written to its own, sees the row, as
    /// `phar_mung_server_vars` has it. `value` is the array's from here;
    /// nothing is written where `$_SERVER` was never made.
    pub fn update_server(&mut self, name: &[u8], value: Owner<Value>) -> Result<(), Thrown> {
        let Some(server) = self.http_globals.server else {
            value.give_up(self);
            return Ok(());
        };
        let name = match PhpString::new(self, name) {
            Ok(name) => name,
            Err(thrown) => {
                value.give_up(self);
                return Err(thrown);
            }
        };
        let written = self.array_update(server.heap(), Key::Str(*name).heap(), value.hand_on());
        name.give_up(self);
        written
    }

    /// `SG(request_info).request_method`, where the host was given one.
    #[must_use]
    pub fn request_method(&self) -> Option<&[u8]> {
        self.web()?.method.as_ref().map(crate::engine::Method::name)
    }

    /// `SG(request_info).request_uri`, where the host was given one: php's
    /// CGI takes it from `SCRIPT_NAME` (`init_request_info`).
    #[must_use]
    pub fn request_uri(&self) -> Option<&[u8]> {
        self.web()?.request_uri.as_deref()
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
    /// merged by `request_order`, or `variables_order` where that has no
    /// value. A host with no request of the web describes none of these,
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
            AutoGlobal::Env if order.contains(&b'E') => {
                let env = self.environment()?;
                self.register_params(*env)?;
                env
            }
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
                self.handle_post(*array, &BodyLimits::default())?;
                array
            }
            AutoGlobal::Files => match self.http_globals.files {
                Some(files) => {
                    self.addref(&Value::array(files));
                    let name = self.declared_string(global.name())?;
                    self.symbol_update(TableId::GLOBAL, name, Value::array(files));
                    return Ok(());
                }
                None => PhpArray::new(self)?,
            },
            AutoGlobal::Request => self.form_variables()?,
            _ => PhpArray::new(self)?,
        };
        let array = array.hand_on();
        let value = Value::array(array);
        if let Some(track) = self.http_globals.track(global) {
            let kept = track.replace(array);
            self.addref(&value);
            if let Some(kept) = kept {
                self.release(Value::array(kept));
            }
        }
        // The auto-global's name, `zend_register_auto_global`'s string,
        // which php interned at startup.
        let name = self.declared_string(global.name())?;
        self.symbol_update(TableId::GLOBAL, name, value);
        Ok(())
    }

    /// The request of the web the host described, where it described one.
    pub(crate) fn web(&self) -> Option<&crate::engine::WebRequest> {
        self.description.web.as_ref()
    }

    /// `php_auto_globals_create_request` (`Z/main/php_variables.c:920`):
    /// `$_GET`, `$_POST` and `$_COOKIE` as their tracks hold them, merged
    /// in the order `request_order` names them, or `variables_order`
    /// where that has no value (`NULL`, [`Request::ini_value`]), each
    /// once; so a `request_order` set empty, by a pool's
    /// `php_admin_value` or a `.user.ini`, merges none of them.
    ///
    /// ```php
    /// echo json_encode([$_GET, $_REQUEST, ini_get('request_order')]), "\n";
    /// ```
    /// ```request fpm
    /// --ENV--
    /// PHP_ADMIN_VALUE=request_order=
    /// --GET--
    /// a=1
    /// ```
    /// ```expect
    /// X-Powered-By: PHP/8.5.9
    /// Content-type: text/html; charset=UTF-8
    ///
    /// [{"a":"1"},[],""]
    /// ```
    fn form_variables(&mut self) -> Result<Owner<PhpArray>, Thrown> {
        let order = match self.ini_value(b"request_order") {
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
    fn merge(
        &mut self,
        mut dest: Owner<PhpArray>,
        source: PhpArray,
    ) -> Result<Owner<PhpArray>, Thrown> {
        // `ZEND_HASH_FOREACH_KEY_VAL(src, num_key, string_key, src_entry)`
        // (`Z/main/php_variables.c:761`), which no code of the program
        // reaches, the request's variables holding strings and arrays: the
        // rows read under the borrowed request, then written.
        let rows: Vec<(Key, Value)> = crate::api::values::walk::rows(self, source)
            .map(|(key, value)| (key, value.deref()))
            .collect();
        for (key, value) in rows {
            let into = dest.get(&key).and_then(|had| had.as_array().copied());
            if let (Some(nested), Some(into)) = (value.as_array().copied(), into) {
                let into = into.copy(self)?;
                let merged = self.merge(into, nested)?;
                let key = Owner::take(self, key);
                dest.set(self, key, merged.into_value())?;
            } else {
                let key = Owner::take(self, key);
                dest.set(self, key, Owner::take(self, value))?;
            }
        }
        Ok(dest)
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
    fn create_server(&mut self) -> Result<Owner<PhpArray>, Thrown> {
        let mut server = self.environment()?;
        self.register_params(*server)?;
        // The host's rows each through the input filter, `PARSE_SERVER`,
        // as `cli_register_variables` and `sapi_cgi_register_variables`
        // hand them, and registered as `php_register_variable_safe` does.
        for (name, value) in self.description.server.clone() {
            if let Some(value) = self.filter_input(Input::Server, &name, &value)? {
                self.register_in(*server, &name, value, false)?;
            }
        }
        // `sapi_get_request_time`, when the request started.
        let now = self.started().as_secs_f64();
        let name = PhpString::new(self, b"REQUEST_TIME_FLOAT")?;
        server.set(self, name.into_key(), Owner::float(now))?;
        let name = PhpString::new(self, b"REQUEST_TIME")?;
        let whole = self.request_time();
        server.set(self, name.into_key(), Owner::int(whole))?;
        // `zend_hash_find_ex_ind` of both, and neither where one is
        // missing: the zval as the table holds it, a reference kept one.
        let found = |req: &mut Request, name: &[u8]| {
            req.symbol_value(TableId::GLOBAL, name)
                .filter(|value| value.tag() != crate::api::values::TypeTag::Undef)
        };
        if !self.description.argv.is_empty() {
            if let (Some(count), Some(words)) = (found(self, b"argc"), found(self, b"argv")) {
                // `ZSTR_KNOWN(ZEND_STR_ARGV)` and `ZEND_STR_ARGC`, the
                // engine's own strings, which cost the request nothing.
                for (name, value) in [(Known::Argv, words), (Known::Argc, count)] {
                    let name = Key::Str(self.known(name)?);
                    server.set(self, Owner::take(self, name), Owner::take(self, value))?;
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
            let query = crate::c_number::c_str(&query);
            let mut words = PhpArray::new(self)?;
            let mut count = 0;
            if !query.is_empty() {
                for word in query.split(|b| *b == b'+') {
                    let word = PhpString::new(self, word)?;
                    words.append(self, word.into_value())?;
                    count += 1;
                }
            }
            let name = PhpString::new(self, b"argv")?;
            server.set(self, name.into_key(), words.into_value())?;
            let name = PhpString::new(self, b"argc")?;
            server.set(self, name.into_key(), Owner::int(count))?;
        }
        self.check_http_proxy(&mut server)?;
        Ok(server)
    }

    /// `check_http_proxy` (`Z/main/php_variables.c:861`): a `HTTP_PROXY`
    /// the request brought replaced by the environment's, or dropped
    /// where the environment has none, lest a request name the process's
    /// proxy (bug #72573).
    fn check_http_proxy(&mut self, server: &mut Owner<PhpArray>) -> Result<(), Thrown> {
        let named = |rows: &[(Vec<u8>, Vec<u8>)]| rows.iter().any(|(n, _)| n == b"HTTP_PROXY");
        if !named(&self.description.params) && !named(&self.description.env) {
            return Ok(());
        }
        let name = PhpString::new(self, b"HTTP_PROXY")?;
        if server.get(&Key::Str(*name)).is_none() {
            name.give_up(self);
            return Ok(());
        }
        let local = self
            .description
            .env
            .iter()
            .find(|(n, _)| n.as_slice() == b"HTTP_PROXY")
            .map(|(_, v)| v.clone());
        if let Some(value) = local {
            let value = PhpString::new(self, &value)?;
            let key = name.into_key();
            server.set(self, key, value.into_value())?;
        } else {
            server.unset(self, &Key::Str(*name))?;
            name.give_up(self);
        }
        Ok(())
    }

    /// `fcgi_loadenv` over `cgi_php_load_env_var`: the request's own
    /// variables, each through the input filter and registered as
    /// `php_register_variable_safe` does, after the environment.
    fn register_params(&mut self, array: PhpArray) -> Result<(), Thrown> {
        for (name, value) in self.description.params.clone() {
            if let Some(value) = self.filter_input(Input::Server, &name, &value)? {
                self.register_in(array, &name, value, false)?;
            }
        }
        Ok(())
    }

    /// `php_import_environment_variables`
    /// (`Z/main/php_variables.c:629`): each variable of the environment
    /// whose name is not empty and holds no space, dot or `[`, under a
    /// name that reads as an integer as that integer, in the order the
    /// environment has them. What `$_ENV` is under a `variables_order`
    /// that names `E`, and the first rows of `$_SERVER`.
    fn environment(&mut self) -> Result<Owner<PhpArray>, Thrown> {
        let mut array = PhpArray::new(self)?;
        for (name, value) in self.description.env.clone() {
            if name.is_empty() || name.iter().any(|b| matches!(b, b' ' | b'.' | b'[')) {
                continue;
            }
            let name = PhpString::new(self, &name)?;
            let name = Key::from_string(self, name);
            let value = match PhpString::new(self, &value) {
                Ok(value) => value,
                Err(thrown) => {
                    name.give_up(self);
                    return Err(thrown);
                }
            };
            array.set(self, name, value.into_value())?;
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
    pub fn parse_query(&mut self, string: &[u8]) -> Result<Owner<PhpArray>, Thrown> {
        let array = PhpArray::new(self)?;
        self.treat_into(Input::String, Some(string), *array, &BodyLimits::default())?;
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
        value: Owner<Value>,
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
    ) -> Result<Owner<PhpArray>, Thrown> {
        let array = PhpArray::new(self)?;
        self.treat_into(input, source, *array, &BodyLimits::default())?;
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
        if self.treated(input, source, into)? {
            return Ok(());
        }
        let source = crate::c_number::c_str(source);
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
                let key = PhpString::new(self, &name)?;
                let held = into.get(&Key::lookup(*key)).is_some();
                key.give_up(self);
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
        if self.treated(Input::Post, body, into)? {
            return Ok(());
        }
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

    /// `sapi_module.treat_data` where an extension registered one
    /// ([`crate::api::abi::ExtensionBuilder::treat_data`]): the variables
    /// the last such extension read of `source`, each handed to the input
    /// filter and registered into `into`, as mbstring's handler hands
    /// each to `sapi_module.input_filter` and `php_register_variable_safe`;
    /// `false` where none takes the source, which is then
    /// `php_default_treat_data`'s.
    fn treated(&mut self, input: Input, source: &[u8], into: PhpArray) -> Result<bool, Thrown> {
        let shared = std::sync::Arc::clone(&self.shared);
        let Some(variables) = shared
            .supplied
            .treat_data
            .entry()
            .map(|(by, hook)| self.serve_for(by, |req| hook(req, input, source)))
            .transpose()?
            .flatten()
        else {
            return Ok(false);
        };
        for (name, value) in variables {
            if let Some(value) = self.filter_input(input, &name, &value)? {
                self.register_in(into, &name, value, false)?;
            }
        }
        Ok(true)
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
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let shared = std::sync::Arc::clone(&self.shared);
        match shared.supplied.input_filter.entry() {
            Some((by, hook)) => self.serve_for(by, |req| hook(req, input, name, value)),
            None => Ok(Some(PhpString::new(self, value)?.into_value())),
        }
    }

    /// `php_register_variable_ex` into `track`, with `cookie` saying the
    /// track is `$_COOKIE`'s, whose plain name already there keeps the
    /// first value (RFC 2965's more specific path first). `value` is the
    /// array's from here, and so is each key the walk makes.
    pub(crate) fn register_in(
        &mut self,
        track: PhpArray,
        var_name: &[u8],
        value: Owner<Value>,
        cookie: bool,
    ) -> Result<(), Thrown> {
        // The name is a C string: it ends at its first NUL.
        let var_name = crate::c_number::c_str(var_name);
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
            value.give_up(self);
            return Ok(());
        }
        let place = Place {
            track,
            original,
            cookie,
        };
        // The walk: `index` is the key the next write lands at, `None`
        // for `[]`; `path` the containers entered so far.
        let index: Option<Vec<u8>> = Some(var[..var_len].to_vec());
        let Some(ip) = bracket else {
            return self.write_at(&place, &[], index.as_deref(), value);
        };
        // The keys of the containers entered, each one owner of its own
        // until the walk ends, so the key names a string that stands.
        let mut path: Vec<Owner<Key>> = Vec::new();
        let walked = self.walk_brackets(&place, &var, var_len, ip, index, &mut path, value);
        for key in path {
            key.give_up(self);
        }
        walked
    }

    /// `php_register_variable_ex`'s walk over the brackets after the
    /// name, from the `[` at `ip`: each index one level of nested array
    /// entered or made, its key kept in `path`, and `value` written at
    /// the last.
    #[allow(clippy::too_many_arguments)]
    fn walk_brackets(
        &mut self,
        place: &Place<'_>,
        var: &[u8],
        var_len: usize,
        mut ip: usize,
        mut index: Option<Vec<u8>>,
        path: &mut Vec<Owner<Key>>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let track = place.track;
        let original = place.original;
        let max_nesting = self.ini_long(b"max_input_nesting_level");
        let mut nest = 0i64;
        loop {
            nest += 1;
            if nest > max_nesting {
                let top = match PhpString::new(self, &var[..var_len]) {
                    Ok(top) => top,
                    Err(thrown) => {
                        value.give_up(self);
                        return Err(thrown);
                    }
                };
                let mut top_level = track;
                let unset = top_level.unset(self, &Key::lookup(*top));
                top.give_up(self);
                value.give_up(self);
                unset?;
                if super::diagnostics::display_errors_mode_of(self.ini_get(b"display_errors")) == 0
                {
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
                return self.write_at(place, path, name.as_deref(), value);
            };
            // Enter, or make, the container at `index`.
            let key = match &index {
                None => None,
                Some(index) => {
                    if forbidden(index, original) {
                        value.give_up(self);
                        return Ok(());
                    }
                    match PhpString::new(self, index) {
                        Ok(index) => Some(Key::from_string(self, index)),
                        Err(thrown) => {
                            value.give_up(self);
                            return Err(thrown);
                        }
                    }
                }
            };
            let entered = match self.enter(track, path, key) {
                Ok(entered) => entered,
                Err(thrown) => {
                    value.give_up(self);
                    return Err(thrown);
                }
            };
            let Some(key) = entered else {
                value.give_up(self);
                return Ok(());
            };
            path.push(key);
            index = new_index;
            ip += 1;
            if var.get(ip) != Some(&b'[') {
                return self.write_at(place, path, index.as_deref(), value);
            }
        }
    }

    /// One step into the walk: the container at `key` (the next index for
    /// `None`), an array made where it is none, and the key it now stands
    /// under; `None` where the next index is taken.
    fn enter(
        &mut self,
        track: PhpArray,
        path: &[Owner<Key>],
        key: Option<Owner<Key>>,
    ) -> Result<Option<Owner<Key>>, Thrown> {
        let mut parent = descend(track, path);
        let Some(key) = key else {
            let fresh = PhpArray::new(self)?;
            if !parent.try_append(self, fresh.into_value())? {
                return Ok(None);
            }
            let last = walk::rows(self, parent).last().map(|(key, _)| key);
            return Ok(last.map(|key| Owner::take(self, key)));
        };
        if parent.get(&key).is_none_or(|v| v.as_array().is_none()) {
            let fresh = match PhpArray::new(self) {
                Ok(fresh) => fresh,
                Err(thrown) => {
                    key.give_up(self);
                    return Err(thrown);
                }
            };
            let slot = Owner::take(self, (*key).clone());
            if let Err(thrown) = parent.set(self, slot, fresh.into_value()) {
                key.give_up(self);
                return Err(thrown);
            }
        }
        Ok(Some(key))
    }

    /// `plain_var:`: the value appended, or written at `index` as a
    /// symtable key, in the container at `path`; a cookie's plain name
    /// already in `$_COOKIE` keeps what it had.
    fn write_at(
        &mut self,
        place: &Place<'_>,
        path: &[Owner<Key>],
        index: Option<&[u8]>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let mut container = descend(place.track, path);
        let Some(index) = index else {
            container.try_append(self, value)?;
            return Ok(());
        };
        if forbidden(index, place.original) {
            value.give_up(self);
            return Ok(());
        }
        let name = match PhpString::new(self, index) {
            Ok(name) => name,
            Err(thrown) => {
                value.give_up(self);
                return Err(thrown);
            }
        };
        if place.cookie && path.is_empty() && container.get(&Key::lookup(*name)).is_some() {
            name.give_up(self);
            value.give_up(self);
            return Ok(());
        }
        let key = Key::from_string(self, name);
        container.set(self, key, value)
    }

    /// The request headers the host received, `getallheaders()`, as
    /// strings of the request's.
    pub fn request_headers(&mut self) -> Result<Owner<PhpArray>, Thrown> {
        let mut array = PhpArray::new(self)?;
        let headers = self
            .description
            .web
            .as_ref()
            .map(|web| web.headers.clone())
            .unwrap_or_default();
        for (name, value) in &headers {
            let key = PhpString::new(self, name)?.into_key();
            let value = match PhpString::new(self, value) {
                Ok(value) => value.into_value(),
                Err(thrown) => {
                    key.give_up(self);
                    return Err(thrown);
                }
            };
            array.set(self, key, value)?;
        }
        Ok(array)
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
fn descend(track: PhpArray, path: &[Owner<Key>]) -> PhpArray {
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

/// `php_url_decode` with `plus`, where `+` is a space, and
/// `php_raw_url_decode` without: `%` and two hex digits a byte, anything
/// else as it is.
#[must_use]
pub fn url_decode(s: &[u8], plus: bool) -> Vec<u8> {
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
