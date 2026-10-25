//! The procedural `mysqli_*` functions, in `mysqli.stub.php`'s order, each
//! the body its method twin serves too.

use php_ext::abi::{ExtensionBuilder, Instance, OrFalse, Ref, True, Union, Untyped, Variadic};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_mysqlnd::client::{CLIENT_INFO, CLIENT_VERSION, Command};

use crate::On;
use crate::extras::{self, mysqli_warning};
use crate::link::{self, Args, Need, mysqli};
use crate::result::{self, AsIs, Column, Fetched, FetchedObject, mysqli_result};
use crate::stmt::{self, mysqli_stmt};

type Link = Instance<mysqli>;
type Res = Instance<mysqli_result>;
type Statement = Instance<mysqli_stmt>;
type Answer<T> = Result<T, Thrown>;
type IntOrString = AsIs<Union<i64, PhpString>>;

fn on(link: Link) -> On {
    On::function(link.object())
}

fn on_result(result: Res) -> On {
    On::function(result.object())
}

fn on_stmt(statement: Statement) -> On {
    On::function(statement.object())
}

/// `ext_functions`, the stub's 106 in its order.
#[allow(clippy::too_many_lines)]
pub(crate) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder.functions(vec![
        mysqli_affected_rows::declared(),
        mysqli_autocommit::declared(),
        mysqli_begin_transaction::declared(),
        mysqli_change_user::declared(),
        mysqli_character_set_name::declared(),
        mysqli_close::declared(),
        mysqli_commit::declared(),
        mysqli_connect::declared(),
        mysqli_connect_errno::declared(),
        mysqli_connect_error::declared(),
        mysqli_data_seek::declared(),
        mysqli_dump_debug_info::declared(),
        mysqli_debug::declared(),
        mysqli_errno::declared(),
        mysqli_error::declared(),
        mysqli_error_list::declared(),
        mysqli_stmt_execute::declared(),
        mysqli_execute::declared(),
        mysqli_execute_query::declared(),
        mysqli_fetch_field::declared(),
        mysqli_fetch_fields::declared(),
        mysqli_fetch_field_direct::declared(),
        mysqli_fetch_lengths::declared(),
        mysqli_fetch_all::declared(),
        mysqli_fetch_array::declared(),
        mysqli_fetch_assoc::declared(),
        mysqli_fetch_object::declared(),
        mysqli_fetch_row::declared(),
        mysqli_fetch_column::declared(),
        mysqli_field_count::declared(),
        mysqli_field_seek::declared(),
        mysqli_field_tell::declared(),
        mysqli_free_result::declared(),
        mysqli_get_connection_stats::declared(),
        mysqli_get_client_stats::declared(),
        mysqli_get_charset::declared(),
        mysqli_get_client_info::declared(),
        mysqli_get_client_version::declared(),
        mysqli_get_links_stats::declared(),
        mysqli_get_host_info::declared(),
        mysqli_get_proto_info::declared(),
        mysqli_get_server_info::declared(),
        mysqli_get_server_version::declared(),
        mysqli_get_warnings::declared(),
        mysqli_init::declared(),
        mysqli_info::declared(),
        mysqli_insert_id::declared(),
        mysqli_kill::declared(),
        mysqli_more_results::declared(),
        mysqli_multi_query::declared(),
        mysqli_next_result::declared(),
        mysqli_num_fields::declared(),
        mysqli_num_rows::declared(),
        mysqli_options::declared(),
        mysqli_set_opt::declared(),
        mysqli_ping::declared(),
        mysqli_poll::declared(),
        mysqli_prepare::declared(),
        mysqli_report::declared(),
        mysqli_query::declared(),
        mysqli_real_connect::declared(),
        mysqli_real_escape_string::declared(),
        mysqli_escape_string::declared(),
        mysqli_real_query::declared(),
        mysqli_reap_async_query::declared(),
        mysqli_release_savepoint::declared(),
        mysqli_rollback::declared(),
        mysqli_savepoint::declared(),
        mysqli_select_db::declared(),
        mysqli_set_charset::declared(),
        mysqli_stmt_affected_rows::declared(),
        mysqli_stmt_attr_get::declared(),
        mysqli_stmt_attr_set::declared(),
        mysqli_stmt_bind_param::declared(),
        mysqli_stmt_bind_result::declared(),
        mysqli_stmt_close::declared(),
        mysqli_stmt_data_seek::declared(),
        mysqli_stmt_errno::declared(),
        mysqli_stmt_error::declared(),
        mysqli_stmt_error_list::declared(),
        mysqli_stmt_fetch::declared(),
        mysqli_stmt_field_count::declared(),
        mysqli_stmt_free_result::declared(),
        mysqli_stmt_get_result::declared(),
        mysqli_stmt_get_warnings::declared(),
        mysqli_stmt_init::declared(),
        mysqli_stmt_insert_id::declared(),
        mysqli_stmt_more_results::declared(),
        mysqli_stmt_next_result::declared(),
        mysqli_stmt_num_rows::declared(),
        mysqli_stmt_param_count::declared(),
        mysqli_stmt_prepare::declared(),
        mysqli_stmt_reset::declared(),
        mysqli_stmt_result_metadata::declared(),
        mysqli_stmt_send_long_data::declared(),
        mysqli_stmt_store_result::declared(),
        mysqli_stmt_sqlstate::declared(),
        mysqli_sqlstate::declared(),
        mysqli_ssl_set::declared(),
        mysqli_stat::declared(),
        mysqli_store_result::declared(),
        mysqli_thread_id::declared(),
        mysqli_thread_safe::declared(),
        mysqli_use_result::declared(),
        mysqli_warning_count::declared(),
        mysqli_refresh::declared(),
    ])
}

/// `mysqli_affected_rows(mysqli $mysql): int|string`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_query($l, 'CREATE TEMPORARY TABLE a (i INT)');
/// mysqli_query($l, 'INSERT INTO a VALUES (1), (2)'); var_dump(mysqli_affected_rows($l));
/// mysqli_query($l, 'SELECT nope'); var_dump(mysqli_affected_rows($l));
/// ```
/// ```expect mysql
/// int(2)
/// int(-1)
/// ```
#[php_function]
fn mysqli_affected_rows(req: &mut Request, mysql: Link) -> Answer<IntOrString> {
    let status = link::status(req, on(mysql))?;
    crate::affected(req, status.affected_rows).map(AsIs::new)
}

/// `mysqli_autocommit(mysqli $mysql, bool $enable): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_autocommit($l, false), mysqli_query($l, 'SELECT @@autocommit')->fetch_row());
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "0"
/// }
/// ```
#[php_function]
fn mysqli_autocommit(req: &mut Request, mysql: Link, enable: bool) -> Answer<bool> {
    link::command(req, on(mysql), Need::Open, Command::Autocommit(enable))
}

/// `mysqli_begin_transaction(mysqli $mysql, int $flags = 0, ?string $name = null): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_begin_transaction($l, MYSQLI_TRANS_START_READ_ONLY, 'n'));
/// try { mysqli_begin_transaction($l, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mysqli_begin_transaction($l, 0, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// mysqli_begin_transaction(): Argument #2 ($flags) must be one of the MYSQLI_TRANS_* constants
/// mysqli_begin_transaction(): Argument #3 ($name) must not be empty
/// ```
#[php_function]
fn mysqli_begin_transaction(
    req: &mut Request,
    mysql: Link,
    #[php(default = 0)] flags: i64,
    #[php(default = null)] name: Option<PhpString>,
) -> Answer<bool> {
    link::begin_transaction(req, on(mysql), flags, name)
}

/// `mysqli_change_user(mysqli $mysql, string $username, string $password, ?string $database): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_change_user($l, 'root', '', null), mysqli_query($l, 'SELECT DATABASE()')->fetch_row());
/// var_dump(@mysqli_change_user($l, 'nobody', 'x', 'test'), mysqli_errno($l));
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   NULL
/// }
/// bool(false)
/// int(1045)
/// ```
#[php_function]
fn mysqli_change_user(
    req: &mut Request,
    mysql: Link,
    username: PhpString,
    #[php(sensitive)] password: PhpString,
    database: Option<PhpString>,
) -> Answer<bool> {
    link::change_user(req, on(mysql), username, password, database)
}

/// `mysqli_character_set_name(mysqli $mysql): string`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_set_charset($l, 'latin1'); var_dump(mysqli_character_set_name($l));
/// ```
/// ```expect mysql
/// string(6) "latin1"
/// ```
#[php_function]
fn mysqli_character_set_name(req: &mut Request, mysql: Link) -> Answer<Owner<PhpString>> {
    let cs = link::status(req, on(mysql))?.charset;
    link::text(req, cs.map(|c| c.name.as_bytes()))
}

/// `mysqli_close(mysqli $mysql): true`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_close($l));
/// try { mysqli_close($l); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(mysqli_close(mysqli_init()));
/// ```
/// ```expect mysql
/// bool(true)
/// mysqli object is already closed
/// bool(true)
/// ```
#[php_function]
fn mysqli_close(req: &mut Request, mysql: Link) -> Answer<True> {
    link::close(req, on(mysql))
}

/// `mysqli_commit(mysqli $mysql, int $flags = 0, ?string $name = null): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_commit($l), mysqli_commit($l, MYSQLI_TRANS_COR_AND_NO_CHAIN));
/// ```
/// ```expect mysql
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn mysqli_commit(
    req: &mut Request,
    mysql: Link,
    #[php(default = 0)] flags: i64,
    #[php(default = null)] name: Option<PhpString>,
) -> Answer<bool> {
    link::end_transaction(req, on(mysql), true, flags, name)
}

/// `mysqli_connect(?string $hostname = null, ...): mysqli|false`.
///
/// ```php
/// var_dump(get_class(mysqli_connect('127.0.0.1', 'root', '', 'test', 3306)));
/// mysqli_report(MYSQLI_REPORT_OFF);
/// var_dump(mysqli_connect('127.0.0.1', 'root', '', 'test', 1));
/// var_dump(mysqli_connect_errno());
/// $p = mysqli_connect('p:127.0.0.1', 'root', '', 'test'); $t = mysqli_thread_id($p); mysqli_close($p);
/// $q = mysqli_connect('p:127.0.0.1', 'root', '', 'test'); var_dump(mysqli_thread_id($q) === $t, mysqli_get_links_stats());
/// ```
/// ```expect mysql
/// string(6) "mysqli"
///
/// Warning: mysqli_connect(): (HY000/2002): Connection refused in %s on line 4
/// bool(false)
/// int(2002)
/// bool(true)
/// array(3) {
///   ["total"]=>
///   int(1)
///   ["active_plinks"]=>
///   int(1)
///   ["cached_plinks"]=>
///   int(0)
/// }
/// ```
///
/// A connection costs the request its link and mysqlnd's structures,
/// strings and stream, and no resource for the stream, whose
/// `zend_resource` `mysqlnd_fixup_regular_list` efrees; its close gives
/// back all but what the link object keeps. The connection
/// `mysqli_init` makes is persistent to mysqlnd and costs nothing more:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $m = memory_get_usage();
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// echo memory_get_usage() - $m, "\n";
/// mysqli_close($l);
/// echo memory_get_usage() - $m, "\n";
/// $l = mysqli_init();
/// $n = memory_get_usage();
/// mysqli_real_connect($l, '127.0.0.1', 'root', '', 'test');
/// echo memory_get_usage() - $n, "\n";
/// ```
/// ```expect mysql
/// 42152
/// 416
/// 0
/// ```
#[php_function]
fn mysqli_connect(
    req: &mut Request,
    #[php(default = null)] hostname: Option<PhpString>,
    #[php(default = null)] username: Option<PhpString>,
    #[php(sensitive, default = null)] password: Option<PhpString>,
    #[php(default = null)] database: Option<PhpString>,
    #[php(default = null)] port: Option<i64>,
    #[php(default = null)] socket: Option<PhpString>,
) -> Answer<OrFalse<Owner<Link>>> {
    let args = Args {
        host: hostname,
        user: username,
        password,
        database,
        port,
        socket,
        flags: 0,
    };
    Ok(match link::common_connect(req, None, args, false, false)? {
        Some(object) => OrFalse::Value(object.into_instance()),
        None => OrFalse::False,
    })
}

/// `mysqli_connect_errno(): int`.
///
/// ```php
/// var_dump(mysqli_connect_errno());
/// mysqli_report(MYSQLI_REPORT_OFF);
/// @mysqli_connect('127.0.0.1', 'nobody', 'x'); var_dump(mysqli_connect_errno());
/// ```
/// ```expect mysql
/// int(0)
/// int(1045)
/// ```
#[php_function]
fn mysqli_connect_errno(req: &mut Request) -> i64 {
    req.state::<crate::State>().connect_errno
}

/// `mysqli_connect_error(): ?string`.
///
/// ```php
/// var_dump(mysqli_connect_error());
/// mysqli_report(MYSQLI_REPORT_OFF);
/// @mysqli_connect('127.0.0.1', 'nobody', 'x'); var_dump(str_starts_with(mysqli_connect_error(), "Access denied for user 'nobody'"));
/// mysqli_connect('127.0.0.1', 'root', '', 'test'); var_dump(mysqli_connect_error());
/// ```
/// ```expect mysql
/// NULL
/// bool(true)
/// NULL
/// ```
#[php_function]
fn mysqli_connect_error(req: &mut Request) -> Answer<Option<Owner<PhpString>>> {
    match req.state::<crate::State>().connect_error.clone() {
        Some(error) => PhpString::new(req, &error).map(Some),
        None => Ok(None),
    }
}

/// `mysqli_data_seek(mysqli_result $result, int $offset): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, 'SELECT 1 UNION SELECT 2');
/// var_dump(mysqli_data_seek($r, 1), mysqli_fetch_row($r), mysqli_data_seek($r, 5));
/// try { mysqli_data_seek($r, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mysqli_data_seek(mysqli_query($l, 'SELECT 1', MYSQLI_USE_RESULT), 0); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "2"
/// }
/// bool(false)
/// mysqli_data_seek(): Argument #2 ($offset) must be greater than or equal to 0
/// mysqli_data_seek() cannot be used in MYSQLI_USE_RESULT mode
/// ```
#[php_function]
fn mysqli_data_seek(req: &mut Request, result: Res, offset: i64) -> Answer<bool> {
    result::data_seek(req, on_result(result), offset)
}

/// `mysqli_dump_debug_info(mysqli $mysql): bool`.
///
/// ```php
/// var_dump(mysqli_dump_debug_info(mysqli_connect('127.0.0.1', 'root', '', 'test')));
/// ```
/// ```expect mysql
/// bool(true)
/// ```
#[php_function]
fn mysqli_dump_debug_info(req: &mut Request, mysql: Link) -> Answer<bool> {
    link::dump_debug_info(req, on(mysql))
}

/// `mysqli_debug(string $options): true`: mysqlnd's tracing is not built,
/// as the oracle's is not.
///
/// ```php
/// var_dump(mysqli_debug('d:t:O,/tmp/client.trace'));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn mysqli_debug(options: PhpString) -> True {
    let _ = options;
    True
}

/// `mysqli_errno(mysqli $mysql): int`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_errno($l)); mysqli_query($l, 'SELECT nope'); var_dump(mysqli_errno($l));
/// ```
/// ```expect mysql
/// int(0)
/// int(1054)
/// ```
#[php_function]
fn mysqli_errno(req: &mut Request, mysql: Link) -> Answer<i64> {
    Ok(i64::from(link::status(req, on(mysql))?.errno))
}

/// `mysqli_error(mysqli $mysql): string`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_error($l)); mysqli_query($l, 'SELECT nope'); var_dump(mysqli_error($l));
/// ```
/// ```expect mysql
/// string(0) ""
/// string(37) "Unknown column 'nope' in 'field list'"
/// ```
#[php_function]
fn mysqli_error(req: &mut Request, mysql: Link) -> Answer<Owner<PhpString>> {
    let error = link::status(req, on(mysql))?.error;
    PhpString::new(req, &error)
}

/// `mysqli_error_list(mysqli $mysql): array`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_error_list($l)); mysqli_query($l, 'SELECT nope'); var_dump(mysqli_error_list($l));
/// ```
/// ```expect mysql
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   array(3) {
///     ["errno"]=>
///     int(1054)
///     ["sqlstate"]=>
///     string(5) "42S22"
///     ["error"]=>
///     string(37) "Unknown column 'nope' in 'field list'"
///   }
/// }
/// ```
#[php_function]
fn mysqli_error_list(req: &mut Request, mysql: Link) -> Answer<Owner<PhpArray>> {
    let list = link::status(req, on(mysql))?.error_list;
    crate::props::error_list(req, &list)
}

/// `mysqli_stmt_execute(mysqli_stmt $statement, ?array $params = null): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $s = mysqli_prepare($l, 'SELECT ? + ?');
/// var_dump(mysqli_stmt_execute($s, [1, 2]), mysqli_stmt_get_result($s)->fetch_row());
/// try { mysqli_stmt_execute($s, ['a' => 1, 'b' => 2]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mysqli_stmt_execute($s, [1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   float(3)
/// }
/// mysqli_stmt_execute(): Argument #2 ($params) must be a list array
/// mysqli_stmt_execute(): Argument #2 ($params) must consist of exactly 2 elements, 1 present
/// ```
#[php_function]
fn mysqli_stmt_execute(
    req: &mut Request,
    statement: Statement,
    #[php(default = null)] params: Option<PhpArray>,
) -> Answer<bool> {
    stmt::execute(req, on_stmt(statement), params)
}

/// `mysqli_execute(mysqli_stmt $statement, ?array $params = null): bool`,
/// deprecated since 8.5.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_execute(mysqli_prepare($l, 'SELECT 1')));
/// ```
/// ```expect mysql
/// Deprecated: Function mysqli_execute() is deprecated since 8.5, use mysqli_stmt_execute() instead in %s on line 3
/// bool(true)
/// ```
#[php_function(deprecated(since = "8.5", message = "use mysqli_stmt_execute() instead"))]
fn mysqli_execute(
    req: &mut Request,
    statement: Statement,
    #[php(default = null)] params: Option<PhpArray>,
) -> Answer<bool> {
    stmt::execute(req, on_stmt(statement), params)
}

/// `mysqli_execute_query(mysqli $mysql, string $query, ?array $params = null): mysqli_result|bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_execute_query($l, 'SELECT ? AS a', ['x'])->fetch_assoc(), mysqli_execute_query($l, 'DO 1'));
/// var_dump(mysqli_execute_query($l, 'SELECT nope'), mysqli_error($l));
/// try { mysqli_execute_query($l, 'SELECT ?', []); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// array(1) {
///   ["a"]=>
///   string(1) "x"
/// }
/// bool(true)
/// bool(false)
/// string(37) "Unknown column 'nope' in 'field list'"
/// mysqli_execute_query(): Argument #3 ($params) must consist of exactly 1 elements, 0 present
/// ```
#[php_function]
fn mysqli_execute_query(
    req: &mut Request,
    mysql: Link,
    query: PhpString,
    #[php(default = null)] params: Option<PhpArray>,
) -> Answer<Union<Owner<Res>, bool>> {
    stmt::execute_query(req, on(mysql), query, params)
}

/// `mysqli_fetch_field(mysqli_result $result): object|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, 'SELECT 1 AS a');
/// var_dump(mysqli_fetch_field($r), mysqli_fetch_field($r));
/// ```
/// ```expect mysql
/// object(stdClass)#3 (13) {
///   ["name"]=>
///   string(1) "a"
///   ["orgname"]=>
///   string(0) ""
///   ["table"]=>
///   string(0) ""
///   ["orgtable"]=>
///   string(0) ""
///   ["def"]=>
///   string(0) ""
///   ["db"]=>
///   string(0) ""
///   ["catalog"]=>
///   string(3) "def"
///   ["max_length"]=>
///   int(0)
///   ["length"]=>
///   int(2)
///   ["charsetnr"]=>
///   int(63)
///   ["flags"]=>
///   int(32897)
///   ["type"]=>
///   int(8)
///   ["decimals"]=>
///   int(0)
/// }
/// bool(false)
/// ```
#[php_function]
fn mysqli_fetch_field(req: &mut Request, result: Res) -> Answer<OrFalse<Owner<PhpObject>>> {
    result::fetch_field(req, on_result(result))
}

/// `mysqli_fetch_fields(mysqli_result $result): array`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// foreach (mysqli_fetch_fields(mysqli_query($l, "SELECT 1 AS a, 'x' AS b")) as $f) echo $f->name, ' ', $f->type, "\n";
/// ```
/// ```expect mysql
/// a 8
/// b 253
/// ```
#[php_function]
fn mysqli_fetch_fields(req: &mut Request, result: Res) -> Answer<Owner<PhpArray>> {
    result::fetch_fields(req, on_result(result))
}

/// `mysqli_fetch_field_direct(mysqli_result $result, int $index): object|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, 'SELECT 1 AS a, 2.5 AS b');
/// var_dump(mysqli_fetch_field_direct($r, 1)->name);
/// try { mysqli_fetch_field_direct($r, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mysqli_fetch_field_direct($r, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// string(1) "b"
/// mysqli_fetch_field_direct(): Argument #2 ($index) must be less than the number of fields for this result set
/// mysqli_fetch_field_direct(): Argument #2 ($index) must be greater than or equal to 0
/// ```
#[php_function]
fn mysqli_fetch_field_direct(
    req: &mut Request,
    result: Res,
    index: i64,
) -> Answer<OrFalse<Owner<PhpObject>>> {
    result::fetch_field_direct(req, on_result(result), index)
}

/// `mysqli_fetch_lengths(mysqli_result $result): array|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, "SELECT 'abc', NULL");
/// var_dump(mysqli_fetch_lengths($r)); mysqli_fetch_row($r); var_dump(mysqli_fetch_lengths($r));
/// ```
/// ```expect mysql
/// bool(false)
/// array(2) {
///   [0]=>
///   int(3)
///   [1]=>
///   int(0)
/// }
/// ```
#[php_function]
fn mysqli_fetch_lengths(req: &mut Request, result: Res) -> Answer<OrFalse<Owner<PhpArray>>> {
    result::fetch_lengths(req, on_result(result))
}

/// `mysqli_fetch_all(mysqli_result $result, int $mode = MYSQLI_NUM): array`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_fetch_all(mysqli_query($l, 'SELECT 1 AS a UNION SELECT 2'), MYSQLI_BOTH));
/// try { mysqli_fetch_all(mysqli_query($l, 'SELECT 1'), 4); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// array(2) {
///   [0]=>
///   array(2) {
///     [0]=>
///     string(1) "1"
///     ["a"]=>
///     string(1) "1"
///   }
///   [1]=>
///   array(2) {
///     [0]=>
///     string(1) "2"
///     ["a"]=>
///     string(1) "2"
///   }
/// }
/// mysqli_fetch_all(): Argument #2 ($mode) must be one of MYSQLI_NUM, MYSQLI_ASSOC, or MYSQLI_BOTH
/// ```
#[php_function]
fn mysqli_fetch_all(
    req: &mut Request,
    result: Res,
    #[php(default = MYSQLI_NUM)] mode: i64,
) -> Answer<Owner<PhpArray>> {
    result::fetch_all(req, on_result(result), mode)
}

/// `mysqli_fetch_array(mysqli_result $result, int $mode = MYSQLI_BOTH): array|null|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, 'SELECT 1 AS a');
/// var_dump(mysqli_fetch_array($r), mysqli_fetch_array($r));
/// try { mysqli_fetch_array($r, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// array(2) {
///   [0]=>
///   string(1) "1"
///   ["a"]=>
///   string(1) "1"
/// }
/// NULL
/// mysqli_fetch_array(): Argument #2 ($mode) must be one of MYSQLI_NUM, MYSQLI_ASSOC, or MYSQLI_BOTH
/// ```
#[php_function]
fn mysqli_fetch_array(
    req: &mut Request,
    result: Res,
    #[php(default = MYSQLI_BOTH)] mode: i64,
) -> Answer<Fetched> {
    result::fetch_array(req, on_result(result), mode)
}

/// `mysqli_fetch_assoc(mysqli_result $result): array|null|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_fetch_assoc(mysqli_query($l, 'SELECT 1 AS a, NULL AS b, 3 AS `3`')));
/// ```
/// ```expect mysql
/// array(3) {
///   ["a"]=>
///   string(1) "1"
///   ["b"]=>
///   NULL
///   [3]=>
///   string(1) "3"
/// }
/// ```
#[php_function]
fn mysqli_fetch_assoc(req: &mut Request, result: Res) -> Answer<Fetched> {
    result::fetch_array(req, on_result(result), result::ASSOC)
}

/// `mysqli_fetch_object(mysqli_result $result, string $class = "stdClass", array $constructor_args = []): object|null|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// class P { public $a; function __construct($x = 0) { echo "ctor $x ", $this->a, "\n"; } }
/// var_dump(mysqli_fetch_object(mysqli_query($l, 'SELECT 1 AS a'), 'P', [7]));
/// var_dump(mysqli_fetch_object(mysqli_query($l, 'SELECT 2 AS a')));
/// try { mysqli_fetch_object(mysqli_query($l, 'SELECT 1'), 'Nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { mysqli_fetch_object(mysqli_query($l, 'SELECT 1'), 'stdClass', [1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// ctor 7 1
/// object(P)#3 (1) {
///   ["a"]=>
///   string(1) "1"
/// }
/// object(stdClass)#2 (1) {
///   ["a"]=>
///   string(1) "2"
/// }
/// mysqli_fetch_object(): Argument #2 ($class) must be a valid class name, Nope given
/// mysqli_fetch_object(): Argument #3 ($constructor_args) must be empty when the specified class (stdClass) does not have a constructor
/// ```
#[php_function]
fn mysqli_fetch_object(
    req: &mut Request,
    result: Res,
    #[php(default = "stdClass")] class: php_ext::abi::Raw<PhpString>,
    #[php(default = [])] constructor_args: php_ext::abi::Raw<PhpArray>,
) -> Answer<FetchedObject> {
    let given = req.frames().argument_count() > 2;
    result::fetch_object(req, on_result(result), class, constructor_args, given)
}

/// `mysqli_fetch_row(mysqli_result $result): array|null|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, "SELECT 1, 'b'");
/// var_dump(mysqli_fetch_row($r), mysqli_fetch_row($r));
/// ```
/// ```expect mysql
/// array(2) {
///   [0]=>
///   string(1) "1"
///   [1]=>
///   string(1) "b"
/// }
/// NULL
/// ```
#[php_function]
fn mysqli_fetch_row(req: &mut Request, result: Res) -> Answer<Fetched> {
    result::fetch_array(req, on_result(result), result::NUM)
}

/// `mysqli_fetch_column(mysqli_result $result, int $column = 0): null|int|float|string|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, "SELECT 1, 'b' UNION SELECT 2, 'c'");
/// var_dump(mysqli_fetch_column($r, 1), mysqli_fetch_column($r), mysqli_fetch_column($r));
/// try { mysqli_fetch_column($r, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// string(1) "b"
/// string(1) "2"
/// bool(false)
/// mysqli_fetch_column(): Argument #2 ($column) must be less than the number of fields for this result set
/// ```
#[php_function]
fn mysqli_fetch_column(
    req: &mut Request,
    result: Res,
    #[php(default = 0)] column: i64,
) -> Answer<Column> {
    result::fetch_column(req, on_result(result), column)
}

/// `mysqli_field_count(mysqli $mysql): int`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_real_query($l, 'SELECT 1, 2, 3'); var_dump(mysqli_field_count($l));
/// ```
/// ```expect mysql
/// int(3)
/// ```
#[php_function]
fn mysqli_field_count(req: &mut Request, mysql: Link) -> Answer<i64> {
    Ok(i64::from(link::status(req, on(mysql))?.field_count))
}

/// `mysqli_field_seek(mysqli_result $result, int $index): true`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, 'SELECT 1 AS a, 2 AS b');
/// var_dump(mysqli_field_seek($r, 1), mysqli_fetch_field($r)->name);
/// try { mysqli_field_seek($r, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// string(1) "b"
/// mysqli_field_seek(): Argument #2 ($index) must be less than the number of fields for this result set
/// ```
#[php_function]
fn mysqli_field_seek(req: &mut Request, result: Res, index: i64) -> Answer<True> {
    result::field_seek(req, on_result(result), index)
}

/// `mysqli_field_tell(mysqli_result $result): int`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, 'SELECT 1 AS a, 2 AS b');
/// var_dump(mysqli_field_tell($r)); mysqli_fetch_field($r); var_dump(mysqli_field_tell($r));
/// ```
/// ```expect mysql
/// int(0)
/// int(1)
/// ```
#[php_function]
fn mysqli_field_tell(req: &mut Request, result: Res) -> Answer<i64> {
    result::with_rows(req, result.object(), |_, set| {
        Ok(i64::try_from(set.field_tell()).unwrap_or(0))
    })
}

/// `mysqli_free_result(mysqli_result $result): void`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $r = mysqli_query($l, 'SELECT 1'); var_dump(mysqli_free_result($r));
/// try { mysqli_free_result($r); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// NULL
/// mysqli_result object is already closed
/// ```
#[php_function]
fn mysqli_free_result(req: &mut Request, result: Res) -> Answer<()> {
    result::free(req, on_result(result))
}

/// `mysqli_get_connection_stats(mysqli $mysql): array`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $s = mysqli_get_connection_stats($l); var_dump(count($s), $s['connect_success']);
/// ```
/// ```expect mysql
/// int(163)
/// string(1) "1"
/// ```
#[php_function]
fn mysqli_get_connection_stats(req: &mut Request, mysql: Link) -> Answer<Owner<PhpArray>> {
    link::connection_stats(req, on(mysql))
}

/// `mysqli_get_client_stats(): array`.
///
/// ```php
/// $s = mysqli_get_client_stats(); var_dump(count($s), $s['bytes_sent']);
/// mysqli_connect('127.0.0.1', 'root', '', 'test'); var_dump(mysqli_get_client_stats()['connect_success']);
/// ```
/// ```expect mysql
/// int(163)
/// string(1) "0"
/// string(1) "1"
/// ```
#[php_function]
fn mysqli_get_client_stats(req: &mut Request) -> Answer<Owner<PhpArray>> {
    extras::client_stats(req)
}

/// `mysqli_get_charset(mysqli $mysql): ?object`.
///
/// ```php
/// var_dump(mysqli_get_charset(mysqli_connect('127.0.0.1', 'root', '', 'test')));
/// ```
/// ```expect mysql
/// object(stdClass)#2 (8) {
///   ["charset"]=>
///   string(7) "utf8mb4"
///   ["collation"]=>
///   string(18) "utf8mb4_0900_ai_ci"
///   ["dir"]=>
///   string(0) ""
///   ["min_length"]=>
///   int(1)
///   ["max_length"]=>
///   int(4)
///   ["number"]=>
///   int(255)
///   ["state"]=>
///   int(1)
///   ["comment"]=>
///   string(0) ""
/// }
/// ```
#[php_function]
fn mysqli_get_charset(req: &mut Request, mysql: Link) -> Answer<Option<Owner<PhpObject>>> {
    link::get_charset(req, on(mysql))
}

/// `mysqli_get_client_info(?mysqli $mysql = null): string`, the link
/// deprecated.
///
/// ```php
/// var_dump(mysqli_get_client_info());
/// var_dump(mysqli_get_client_info(mysqli_init()));
/// ```
/// ```expect
/// string(14) "mysqlnd 8.5.12"
///
/// Deprecated: mysqli_get_client_info(): Passing connection object as an argument is deprecated in %s on line 3
/// string(14) "mysqlnd 8.5.12"
/// ```
#[php_function]
fn mysqli_get_client_info(
    req: &mut Request,
    #[php(default = null)] mysql: Option<Link>,
) -> Answer<Owner<PhpString>> {
    let _ = mysql;
    if req.frames().argument_count() > 0 {
        docref(
            req,
            Level::Deprecated,
            "Passing connection object as an argument is deprecated",
        );
    }
    PhpString::new(req, CLIENT_INFO.as_bytes())
}

/// `mysqli_get_client_version(): int`.
///
/// ```php
/// var_dump(mysqli_get_client_version());
/// ```
/// ```expect
/// int(80512)
/// ```
#[php_function]
fn mysqli_get_client_version() -> i64 {
    i64::try_from(CLIENT_VERSION).unwrap_or(0)
}

/// `mysqli_get_links_stats(): array`.
///
/// ```php
/// var_dump(mysqli_get_links_stats());
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test'); var_dump(mysqli_get_links_stats()['total']);
/// ```
/// ```expect mysql
/// array(3) {
///   ["total"]=>
///   int(0)
///   ["active_plinks"]=>
///   int(0)
///   ["cached_plinks"]=>
///   int(0)
/// }
/// int(1)
/// ```
#[php_function]
fn mysqli_get_links_stats(req: &mut Request) -> Answer<Owner<PhpArray>> {
    link::links_stats(req)
}

/// `mysqli_get_host_info(mysqli $mysql): string`.
///
/// ```php
/// var_dump(mysqli_get_host_info(mysqli_connect('127.0.0.1', 'root', '', 'test')));
/// ```
/// ```expect mysql
/// string(20) "127.0.0.1 via TCP/IP"
/// ```
#[php_function]
fn mysqli_get_host_info(req: &mut Request, mysql: Link) -> Answer<Owner<PhpString>> {
    let info = link::status(req, on(mysql))?.host_info;
    link::text(req, info.as_deref())
}

/// `mysqli_get_proto_info(mysqli $mysql): int`.
///
/// ```php
/// var_dump(mysqli_get_proto_info(mysqli_connect('127.0.0.1', 'root', '', 'test')));
/// ```
/// ```expect mysql
/// int(10)
/// ```
#[php_function]
fn mysqli_get_proto_info(req: &mut Request, mysql: Link) -> Answer<i64> {
    Ok(i64::from(link::status(req, on(mysql))?.proto_info))
}

/// `mysqli_get_server_info(mysqli $mysql): string`.
///
/// ```php
/// var_dump(str_starts_with(mysqli_get_server_info(mysqli_connect('127.0.0.1', 'root', '', 'test')), '8.'));
/// ```
/// ```expect mysql
/// bool(true)
/// ```
#[php_function]
fn mysqli_get_server_info(req: &mut Request, mysql: Link) -> Answer<Owner<PhpString>> {
    let info = link::status(req, on(mysql))?.server_info;
    link::text(req, info.as_deref())
}

/// `mysqli_get_server_version(mysqli $mysql): int`.
///
/// ```php
/// var_dump(mysqli_get_server_version(mysqli_connect('127.0.0.1', 'root', '', 'test')) >= 80000);
/// ```
/// ```expect mysql
/// bool(true)
/// ```
#[php_function]
fn mysqli_get_server_version(req: &mut Request, mysql: Link) -> Answer<i64> {
    Ok(i64::try_from(link::status(req, on(mysql))?.server_version).unwrap_or(0))
}

/// `mysqli_get_warnings(mysqli $mysql): mysqli_warning|false`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_get_warnings($l)); mysqli_query($l, 'SELECT 1/0'); var_dump(mysqli_get_warnings($l)->errno);
/// ```
/// ```expect mysql
/// bool(false)
/// int(1365)
/// ```
#[php_function]
fn mysqli_get_warnings(
    req: &mut Request,
    mysql: Link,
) -> Answer<OrFalse<Owner<Instance<mysqli_warning>>>> {
    extras::link_warnings(req, on(mysql))
}

/// `mysqli_init(): mysqli|false`.
///
/// ```php
/// $l = mysqli_init(); var_dump(get_class($l));
/// try { $l->query('SELECT 1'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "mysqli"
/// mysqli object is not fully initialized
/// ```
#[php_function]
fn mysqli_init(req: &mut Request) -> Answer<OrFalse<Owner<Link>>> {
    link::init(req, None).map(|o| OrFalse::Value(o.into_instance()))
}

/// `mysqli_info(mysqli $mysql): ?string`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_query($l, 'CREATE TEMPORARY TABLE i (a INT)');
/// var_dump(mysqli_info($l)); mysqli_query($l, 'INSERT INTO i VALUES (1), (2)'); var_dump(mysqli_info($l));
/// ```
/// ```expect mysql
/// NULL
/// string(38) "Records: 2  Duplicates: 0  Warnings: 0"
/// ```
#[php_function]
fn mysqli_info(req: &mut Request, mysql: Link) -> Answer<Option<Owner<PhpString>>> {
    match link::status(req, on(mysql))?.info {
        Some(info) => PhpString::new(req, &info).map(Some),
        None => Ok(None),
    }
}

/// `mysqli_insert_id(mysqli $mysql): int|string`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_query($l, 'CREATE TEMPORARY TABLE n (id INT AUTO_INCREMENT PRIMARY KEY)');
/// var_dump(mysqli_insert_id($l)); mysqli_query($l, 'INSERT INTO n VALUES (41), (NULL)'); var_dump(mysqli_insert_id($l));
/// ```
/// ```expect mysql
/// int(0)
/// int(42)
/// ```
#[php_function]
fn mysqli_insert_id(req: &mut Request, mysql: Link) -> Answer<IntOrString> {
    let id = link::status(req, on(mysql))?.insert_id;
    crate::long_or_string(req, id).map(AsIs::new)
}

/// `mysqli_kill(mysqli $mysql, int $process_id): bool`, deprecated since 8.4.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $k = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_kill($l, mysqli_thread_id($k)));
/// try { mysqli_kill($l, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// Deprecated: Function mysqli_kill() is deprecated since 8.4, use KILL CONNECTION/QUERY SQL statement instead in %s on line 5
/// bool(false)
///
/// Deprecated: Function mysqli_kill() is deprecated since 8.4, use KILL CONNECTION/QUERY SQL statement instead in %s on line 6
/// mysqli_kill(): Argument #2 ($process_id) must be greater than 0
/// ```
#[php_function(deprecated(
    since = "8.4",
    message = "use KILL CONNECTION/QUERY SQL statement instead"
))]
fn mysqli_kill(req: &mut Request, mysql: Link, process_id: i64) -> Answer<bool> {
    link::kill(req, on(mysql), process_id)
}

/// `mysqli_more_results(mysqli $mysql): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_more_results($l)); mysqli_multi_query($l, 'SELECT 1; SELECT 2'); var_dump(mysqli_more_results($l));
/// ```
/// ```expect mysql
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn mysqli_more_results(req: &mut Request, mysql: Link) -> Answer<bool> {
    Ok(link::status(req, on(mysql))?.more_results)
}

/// `mysqli_multi_query(mysqli $mysql, string $query): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_multi_query($l, 'SELECT 1; SELECT 2'));
/// do { var_dump(mysqli_store_result($l)->fetch_row()); } while (mysqli_next_result($l));
/// var_dump(mysqli_multi_query($l, 'SELEC 1; SELECT 2'), mysqli_errno($l));
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "1"
/// }
/// array(1) {
///   [0]=>
///   string(1) "2"
/// }
/// bool(false)
/// int(1064)
/// ```
#[php_function]
fn mysqli_multi_query(req: &mut Request, mysql: Link, query: PhpString) -> Answer<bool> {
    link::multi_query(req, on(mysql), query)
}

/// `mysqli_next_result(mysqli $mysql): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_multi_query($l, 'SELECT 1; SELECT 2'); mysqli_store_result($l);
/// var_dump(mysqli_next_result($l), mysqli_store_result($l)->fetch_row(), mysqli_next_result($l));
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "2"
/// }
/// bool(false)
/// ```
#[php_function]
fn mysqli_next_result(req: &mut Request, mysql: Link) -> Answer<bool> {
    link::next_result(req, on(mysql))
}

/// `mysqli_num_fields(mysqli_result $result): int`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_num_fields(mysqli_query($l, 'SELECT 1, 2')));
/// ```
/// ```expect mysql
/// int(2)
/// ```
#[php_function]
fn mysqli_num_fields(req: &mut Request, result: Res) -> Answer<i64> {
    result::count(
        req,
        on_result(result),
        php_ext_mysqlnd::client::ResultSet::num_fields,
    )
}

/// `mysqli_num_rows(mysqli_result $result): int|string`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_num_rows(mysqli_query($l, 'SELECT 1 UNION SELECT 2')));
/// $r = mysqli_query($l, 'SELECT 1', MYSQLI_USE_RESULT);
/// try { mysqli_num_rows($r); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// while (mysqli_fetch_row($r)); var_dump(mysqli_num_rows($r));
/// ```
/// ```expect mysql
/// int(2)
/// mysqli_num_rows() cannot be used in MYSQLI_USE_RESULT mode
/// int(1)
/// ```
#[php_function]
fn mysqli_num_rows(req: &mut Request, result: Res) -> Answer<IntOrString> {
    result::num_rows(req, on_result(result))
}

/// `mysqli_options(mysqli $mysql, int $option, $value): bool`.
///
/// ```php
/// $l = mysqli_init();
/// var_dump(mysqli_options($l, MYSQLI_INIT_COMMAND, 'SET @w = 5'), mysqli_options($l, MYSQLI_OPT_CONNECT_TIMEOUT, '10'), mysqli_options($l, 9999, 1));
/// mysqli_real_connect($l, '127.0.0.1', 'root', '', 'test'); var_dump(mysqli_query($l, 'SELECT @w')->fetch_row());
/// ```
/// ```expect mysql
/// bool(true)
/// bool(true)
/// bool(false)
/// array(1) {
///   [0]=>
///   string(1) "5"
/// }
/// ```
#[php_function]
fn mysqli_options(
    req: &mut Request,
    mysql: Link,
    option: i64,
    value: Untyped<Value>,
) -> Answer<bool> {
    link::options(req, on(mysql), option, value.0)
}

/// `mysqli_set_opt(mysqli $mysql, int $option, $value): bool`.
///
/// ```php
/// var_dump(mysqli_set_opt(mysqli_init(), MYSQLI_OPT_INT_AND_FLOAT_NATIVE, true));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn mysqli_set_opt(
    req: &mut Request,
    mysql: Link,
    option: i64,
    value: Untyped<Value>,
) -> Answer<bool> {
    link::options(req, on(mysql), option, value.0)
}

/// `mysqli_ping(mysqli $mysql): bool`, deprecated since 8.4.
///
/// ```php
/// var_dump(mysqli_ping(mysqli_connect('127.0.0.1', 'root', '', 'test')));
/// ```
/// ```expect mysql
/// Deprecated: Function mysqli_ping() is deprecated since 8.4, because the reconnect feature has been removed in PHP 8.2 and this function is now redundant in %s on line 2
/// bool(true)
/// ```
#[php_function(deprecated(
    since = "8.4",
    message = "because the reconnect feature has been removed in PHP 8.2 and this function is now redundant"
))]
fn mysqli_ping(req: &mut Request, mysql: Link) -> Answer<bool> {
    link::ping(req, on(mysql))
}

/// `mysqli_poll(?array &$read, ?array &$error, array &$reject, int $seconds, int $microseconds = 0): int|false`.
///
/// ```php
/// $a = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $b = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_query($a, 'SELECT 1', MYSQLI_ASYNC);
/// $r = [$a, $b]; $e = []; $x = [];
/// var_dump(mysqli_poll($r, $e, $x, 1), count($r), count($x), $x[0] === $b);
/// var_dump(mysqli_reap_async_query($a)->fetch_row());
/// try { mysqli_poll($n, $n, $x, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { $r = [1]; mysqli_poll($r, $e, $x, 0); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// int(1)
/// int(1)
/// int(1)
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "1"
/// }
/// No stream arrays were passed
/// mysqli_poll(): Argument #2 ($error) must be of type ?array, ValueError given
/// ```
#[php_function]
fn mysqli_poll(
    req: &mut Request,
    read: Ref<Option<PhpArray>>,
    error: Ref<Option<PhpArray>>,
    reject: Ref<PhpArray>,
    seconds: i64,
    #[php(default = 0)] microseconds: i64,
) -> Answer<OrFalse<i64>> {
    extras::poll(req, &read, &error, &reject, seconds, microseconds)
}

/// `mysqli_prepare(mysqli $mysql, string $query): mysqli_stmt|false`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(get_class(mysqli_prepare($l, 'SELECT ?')), mysqli_prepare($l, 'SELEC'), mysqli_errno($l));
/// ```
/// ```expect mysql
/// string(11) "mysqli_stmt"
/// bool(false)
/// int(1064)
/// ```
#[php_function]
fn mysqli_prepare(
    req: &mut Request,
    mysql: Link,
    query: PhpString,
) -> Answer<OrFalse<Owner<Statement>>> {
    stmt::prepare(req, on(mysql), query)
}

/// `mysqli_report(int $flags): true`.
///
/// ```php
/// var_dump(mysqli_report(MYSQLI_REPORT_ERROR), (new mysqli_driver)->report_mode);
/// mysqli_report(MYSQLI_REPORT_ERROR);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_query($l, 'SELECT nope');
/// mysqli_report(MYSQLI_REPORT_STRICT | MYSQLI_REPORT_ERROR);
/// try { mysqli_query($l, 'SELECT nope'); } catch (mysqli_sql_exception $e) { echo $e->getCode(), ' ', $e->getSqlState(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// int(1)
///
/// Warning: mysqli_query(): (42S22/1054): Unknown column 'nope' in 'field list' in %s on line 5
/// 1054 42S22
/// ```
#[php_function]
fn mysqli_report(req: &mut Request, flags: i64) -> True {
    extras::set_report(req, flags);
    True
}

/// `mysqli_query(mysqli $mysql, string $query, int $result_mode = MYSQLI_STORE_RESULT): mysqli_result|bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_query($l, 'DO 1'), mysqli_query($l, 'SELECT 1')->num_rows, mysqli_query($l, 'SELECT nope'));
/// try { mysqli_query($l, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mysqli_query($l, 'SELECT 1', 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// int(1)
/// bool(false)
/// mysqli_query(): Argument #2 ($query) must not be empty
/// mysqli_query(): Argument #3 ($result_mode) must be either MYSQLI_USE_RESULT or MYSQLI_STORE_RESULT with MYSQLI_ASYNC as an optional bitmask flag
/// ```
#[php_function]
fn mysqli_query(
    req: &mut Request,
    mysql: Link,
    query: PhpString,
    #[php(default = MYSQLI_STORE_RESULT)] result_mode: i64,
) -> Answer<Union<Owner<Res>, bool>> {
    link::query(req, on(mysql), query, result_mode)
}

/// `mysqli_real_connect(mysqli $mysql, ?string $hostname = null, ..., int $flags = 0): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_init();
/// var_dump(@mysqli_real_connect($l, '127.0.0.1', 'nobody', 'x'), mysqli_real_connect($l, '127.0.0.1', 'root', '', 'test', 3306, null, MYSQLI_CLIENT_FOUND_ROWS));
/// var_dump(mysqli_query($l, 'SELECT 1')->fetch_row());
/// ```
/// ```expect mysql
/// bool(false)
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "1"
/// }
/// ```
#[php_function]
fn mysqli_real_connect(
    req: &mut Request,
    mysql: Link,
    #[php(default = null)] hostname: Option<PhpString>,
    #[php(default = null)] username: Option<PhpString>,
    #[php(sensitive, default = null)] password: Option<PhpString>,
    #[php(default = null)] database: Option<PhpString>,
    #[php(default = null)] port: Option<i64>,
    #[php(default = null)] socket: Option<PhpString>,
    #[php(default = 0)] flags: i64,
) -> Answer<bool> {
    let args = Args {
        host: hostname,
        user: username,
        password,
        database,
        port,
        socket,
        flags,
    };
    let made = link::common_connect(req, Some(mysql.object()), args, true, false)?;
    Ok(link::connected_link(req, made))
}

/// `mysqli_real_escape_string(mysqli $mysql, string $string): string`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_real_escape_string($l, "a'b\"c\\\n\0"));
/// ```
/// ```expect mysql
/// string(13) "a\'b\"c\\\n\0"
/// ```
#[php_function]
fn mysqli_real_escape_string(
    req: &mut Request,
    mysql: Link,
    string: PhpString,
) -> Answer<Owner<PhpString>> {
    link::escape(req, on(mysql), string)
}

/// `mysqli_escape_string(mysqli $mysql, string $string): string`.
///
/// ```php
/// var_dump(mysqli_escape_string(mysqli_connect('127.0.0.1', 'root', '', 'test'), "it's"));
/// ```
/// ```expect mysql
/// string(5) "it\'s"
/// ```
#[php_function]
fn mysqli_escape_string(
    req: &mut Request,
    mysql: Link,
    string: PhpString,
) -> Answer<Owner<PhpString>> {
    link::escape(req, on(mysql), string)
}

/// `mysqli_real_query(mysqli $mysql, string $query): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_real_query($l, 'SELECT 1'), mysqli_use_result($l)->fetch_row(), mysqli_real_query($l, 'SELEC'));
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "1"
/// }
/// bool(false)
/// ```
#[php_function]
fn mysqli_real_query(req: &mut Request, mysql: Link, query: PhpString) -> Answer<bool> {
    link::real_query(req, on(mysql), query)
}

/// `mysqli_reap_async_query(mysqli $mysql): mysqli_result|bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_query($l, 'SELECT 1', MYSQLI_ASYNC | MYSQLI_USE_RESULT), mysqli_reap_async_query($l)->fetch_row());
/// mysqli_query($l, 'DO 1', MYSQLI_ASYNC); var_dump(mysqli_reap_async_query($l));
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   string(1) "1"
/// }
/// bool(true)
/// ```
#[php_function]
fn mysqli_reap_async_query(req: &mut Request, mysql: Link) -> Answer<Union<Owner<Res>, bool>> {
    link::reap_async_query(req, on(mysql))
}

/// `mysqli_release_savepoint(mysqli $mysql, string $name): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_begin_transaction($l); mysqli_savepoint($l, 'p');
/// var_dump(mysqli_release_savepoint($l, 'p'));
/// try { mysqli_release_savepoint($l, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// mysqli_release_savepoint(): Argument #2 ($name) must not be empty
/// ```
#[php_function]
fn mysqli_release_savepoint(req: &mut Request, mysql: Link, name: PhpString) -> Answer<bool> {
    link::savepoint(req, on(mysql), name, true)
}

/// `mysqli_rollback(mysqli $mysql, int $flags = 0, ?string $name = null): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_rollback($l), mysqli_rollback($l, MYSQLI_TRANS_COR_RELEASE));
/// ```
/// ```expect mysql
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn mysqli_rollback(
    req: &mut Request,
    mysql: Link,
    #[php(default = 0)] flags: i64,
    #[php(default = null)] name: Option<PhpString>,
) -> Answer<bool> {
    link::end_transaction(req, on(mysql), false, flags, name)
}

/// `mysqli_savepoint(mysqli $mysql, string $name): bool`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_begin_transaction($l); var_dump(mysqli_savepoint($l, 'p'));
/// try { mysqli_savepoint($l, ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// mysqli_savepoint(): Argument #2 ($name) must not be empty
/// ```
#[php_function]
fn mysqli_savepoint(req: &mut Request, mysql: Link, name: PhpString) -> Answer<bool> {
    link::savepoint(req, on(mysql), name, false)
}

/// `mysqli_select_db(mysqli $mysql, string $database): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_select_db($l, 'mysql'), mysqli_query($l, 'SELECT DATABASE()')->fetch_row(), mysqli_select_db($l, 'nope_db'));
/// ```
/// ```expect mysql
/// bool(true)
/// array(1) {
///   [0]=>
///   string(5) "mysql"
/// }
/// bool(false)
/// ```
#[php_function]
fn mysqli_select_db(req: &mut Request, mysql: Link, database: PhpString) -> Answer<bool> {
    link::command(
        req,
        on(mysql),
        Need::Open,
        Command::SelectDb(database.as_bytes()),
    )
}

/// `mysqli_set_charset(mysqli $mysql, string $charset): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_set_charset($l, 'latin1'), mysqli_character_set_name($l), @mysqli_set_charset($l, 'nope'));
/// ```
/// ```expect mysql
/// bool(true)
/// string(6) "latin1"
/// bool(false)
/// ```
#[php_function]
fn mysqli_set_charset(req: &mut Request, mysql: Link, charset: PhpString) -> Answer<bool> {
    link::command(
        req,
        on(mysql),
        Need::Open,
        Command::SetCharset(charset.as_bytes()),
    )
}

/// `mysqli_stmt_affected_rows(mysqli_stmt $statement): int|string`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_query($l, 'CREATE TEMPORARY TABLE sa (i INT)');
/// $s = mysqli_prepare($l, 'INSERT INTO sa VALUES (1), (2)'); mysqli_stmt_execute($s);
/// var_dump(mysqli_stmt_affected_rows($s));
/// ```
/// ```expect mysql
/// int(2)
/// ```
#[php_function]
fn mysqli_stmt_affected_rows(req: &mut Request, statement: Statement) -> Answer<IntOrString> {
    stmt::affected_rows(req, on_stmt(statement))
}

/// `mysqli_stmt_attr_get(mysqli_stmt $statement, int $attribute): int`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// var_dump(mysqli_stmt_attr_get($s, MYSQLI_STMT_ATTR_CURSOR_TYPE));
/// try { mysqli_stmt_attr_get($s, 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// int(0)
/// mysqli_stmt_attr_get(): Argument #2 ($attribute) must be either MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH or MYSQLI_STMT_ATTR_CURSOR_TYPE
/// ```
#[php_function]
fn mysqli_stmt_attr_get(req: &mut Request, statement: Statement, attribute: i64) -> Answer<i64> {
    stmt::attr_get(req, on_stmt(statement), attribute)
}

/// `mysqli_stmt_attr_set(mysqli_stmt $statement, int $attribute, int $value): bool`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// var_dump(mysqli_stmt_attr_set($s, MYSQLI_STMT_ATTR_CURSOR_TYPE, MYSQLI_CURSOR_TYPE_READ_ONLY), mysqli_stmt_attr_get($s, MYSQLI_STMT_ATTR_CURSOR_TYPE));
/// try { mysqli_stmt_attr_set($s, MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mysqli_stmt_attr_set($s, 7, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// int(1)
/// mysqli_stmt_attr_set(): Argument #3 ($value) must be 0 or 1 for attribute MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH
/// mysqli_stmt_attr_set(): Argument #2 ($attribute) must be either MYSQLI_STMT_ATTR_UPDATE_MAX_LENGTH or MYSQLI_STMT_ATTR_CURSOR_TYPE
/// ```
#[php_function]
fn mysqli_stmt_attr_set(
    req: &mut Request,
    statement: Statement,
    attribute: i64,
    value: i64,
) -> Answer<bool> {
    stmt::attr_set(req, on_stmt(statement), attribute, value)
}

/// `mysqli_stmt_bind_param(mysqli_stmt $statement, string $types, mixed &...$vars): bool`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT ?, ?, ?');
/// $a = 1; $b = 2.5; $c = 'x';
/// var_dump(mysqli_stmt_bind_param($s, 'ids', $a, $b, $c)); $c = 'y'; mysqli_stmt_execute($s);
/// var_dump(mysqli_stmt_get_result($s)->fetch_row());
/// try { mysqli_stmt_bind_param($s, 'i', $a); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { mysqli_stmt_bind_param($s, 'iix', $a, $b, $c); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   float(2.5)
///   [2]=>
///   string(1) "y"
/// }
/// The number of variables must match the number of parameters in the prepared statement
/// mysqli_stmt_bind_param(): Argument #2 ($types) must only contain the "b", "d", "i", "s" type specifiers
/// ```
#[php_function]
fn mysqli_stmt_bind_param(
    req: &mut Request,
    statement: Statement,
    types: PhpString,
    vars: Variadic<Ref<Value>>,
) -> Answer<bool> {
    stmt::bind_param(req, on_stmt(statement), types, &vars)
}

/// `mysqli_stmt_bind_result(mysqli_stmt $statement, mixed &...$vars): bool`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), "SELECT 1, 'a'");
/// mysqli_stmt_execute($s); var_dump(mysqli_stmt_bind_result($s, $n, $t), mysqli_stmt_fetch($s), $n, $t);
/// try { mysqli_stmt_bind_result($s, $n); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// bool(true)
/// int(1)
/// string(1) "a"
/// Number of bind variables doesn't match number of fields in prepared statement
/// ```
#[php_function]
fn mysqli_stmt_bind_result(
    req: &mut Request,
    statement: Statement,
    vars: Variadic<Ref<Value>>,
) -> Answer<bool> {
    stmt::bind_result(req, on_stmt(statement), &vars)
}

/// `mysqli_stmt_close(mysqli_stmt $statement): true`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// var_dump(mysqli_stmt_close($s));
/// try { mysqli_stmt_close($s); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// mysqli_stmt object is already closed
/// ```
#[php_function]
fn mysqli_stmt_close(req: &mut Request, statement: Statement) -> Answer<True> {
    stmt::close(req, on_stmt(statement))
}

/// `mysqli_stmt_data_seek(mysqli_stmt $statement, int $offset): void`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1 UNION SELECT 2');
/// mysqli_stmt_execute($s); mysqli_stmt_store_result($s); mysqli_stmt_bind_result($s, $v);
/// mysqli_stmt_data_seek($s, 1); mysqli_stmt_fetch($s); var_dump($v);
/// try { mysqli_stmt_data_seek($s, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// int(2)
/// mysqli_stmt_data_seek(): Argument #2 ($offset) must be greater than or equal to 0
/// ```
#[php_function]
fn mysqli_stmt_data_seek(req: &mut Request, statement: Statement, offset: i64) -> Answer<()> {
    stmt::data_seek(req, on_stmt(statement), offset)
}

/// `mysqli_stmt_errno(mysqli_stmt $statement): int`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $s = mysqli_stmt_init($l = mysqli_connect('127.0.0.1', 'root', '', 'test'));
/// var_dump(mysqli_stmt_errno($s)); mysqli_stmt_prepare($s, 'SELEC'); var_dump(mysqli_stmt_errno($s));
/// ```
/// ```expect mysql
/// int(0)
/// int(1064)
/// ```
#[php_function]
fn mysqli_stmt_errno(req: &mut Request, statement: Statement) -> Answer<i64> {
    Ok(i64::from(
        stmt::error_facts(req, on_stmt(statement), Need::Made)?.errno,
    ))
}

/// `mysqli_stmt_error(mysqli_stmt $statement): string`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $s = mysqli_stmt_init($l = mysqli_connect('127.0.0.1', 'root', '', 'test'));
/// var_dump(mysqli_stmt_error($s)); mysqli_stmt_prepare($s, 'SELECT nope'); var_dump(mysqli_stmt_error($s));
/// ```
/// ```expect mysql
/// string(0) ""
/// string(37) "Unknown column 'nope' in 'field list'"
/// ```
#[php_function]
fn mysqli_stmt_error(req: &mut Request, statement: Statement) -> Answer<Owner<PhpString>> {
    let error = stmt::error_facts(req, on_stmt(statement), Need::Made)?.error;
    PhpString::new(req, &error)
}

/// `mysqli_stmt_error_list(mysqli_stmt $statement): array`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $s = mysqli_stmt_init($l = mysqli_connect('127.0.0.1', 'root', '', 'test'));
/// mysqli_stmt_prepare($s, 'SELECT nope'); var_dump(mysqli_stmt_error_list($s));
/// ```
/// ```expect mysql
/// array(1) {
///   [0]=>
///   array(3) {
///     ["errno"]=>
///     int(1054)
///     ["sqlstate"]=>
///     string(5) "42S22"
///     ["error"]=>
///     string(37) "Unknown column 'nope' in 'field list'"
///   }
/// }
/// ```
#[php_function]
fn mysqli_stmt_error_list(req: &mut Request, statement: Statement) -> Answer<Owner<PhpArray>> {
    stmt::error_list(req, on_stmt(statement))
}

/// `mysqli_stmt_fetch(mysqli_stmt $statement): ?bool`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// mysqli_stmt_execute($s); mysqli_stmt_bind_result($s, $v);
/// var_dump(mysqli_stmt_fetch($s), $v, mysqli_stmt_fetch($s));
/// ```
/// ```expect mysql
/// bool(true)
/// int(1)
/// NULL
/// ```
#[php_function]
fn mysqli_stmt_fetch(req: &mut Request, statement: Statement) -> Answer<Option<bool>> {
    stmt::fetch(req, on_stmt(statement))
}

/// `mysqli_stmt_field_count(mysqli_stmt $statement): int`.
///
/// ```php
/// var_dump(mysqli_stmt_field_count(mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1, 2')));
/// ```
/// ```expect mysql
/// int(2)
/// ```
#[php_function]
fn mysqli_stmt_field_count(req: &mut Request, statement: Statement) -> Answer<i64> {
    Ok(i64::from(
        stmt::error_facts(req, on_stmt(statement), Need::Open)?.field_count,
    ))
}

/// `mysqli_stmt_free_result(mysqli_stmt $statement): void`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// mysqli_stmt_execute($s); mysqli_stmt_store_result($s);
/// var_dump(mysqli_stmt_free_result($s), mysqli_stmt_num_rows($s));
/// ```
/// ```expect mysql
/// NULL
/// int(0)
/// ```
#[php_function]
fn mysqli_stmt_free_result(req: &mut Request, statement: Statement) -> Answer<()> {
    stmt::free_result(req, on_stmt(statement))
}

/// `mysqli_stmt_get_result(mysqli_stmt $statement): mysqli_result|false`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1 AS a');
/// mysqli_stmt_execute($s); var_dump(mysqli_stmt_get_result($s)->fetch_assoc(), mysqli_stmt_get_result($s));
/// ```
/// ```expect mysql
/// array(1) {
///   ["a"]=>
///   int(1)
/// }
/// bool(false)
/// ```
#[php_function]
fn mysqli_stmt_get_result(req: &mut Request, statement: Statement) -> Answer<OrFalse<Owner<Res>>> {
    stmt::get_result(req, on_stmt(statement))
}

/// `mysqli_stmt_get_warnings(mysqli_stmt $statement): mysqli_warning|false`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1/0');
/// var_dump(mysqli_stmt_get_warnings($s)); mysqli_stmt_execute($s); var_dump(mysqli_stmt_get_warnings($s)->errno);
/// ```
/// ```expect mysql
/// bool(false)
///
/// Warning: Attempt to read property "errno" on false in %s on line 3
/// NULL
/// ```
#[php_function]
fn mysqli_stmt_get_warnings(
    req: &mut Request,
    statement: Statement,
) -> Answer<OrFalse<Owner<Instance<mysqli_warning>>>> {
    extras::stmt_warnings(req, on_stmt(statement))
}

/// `mysqli_stmt_init(mysqli $mysql): mysqli_stmt|false`.
///
/// ```php
/// $s = mysqli_stmt_init($l = mysqli_connect('127.0.0.1', 'root', '', 'test'));
/// var_dump(get_class($s));
/// try { mysqli_stmt_execute($s); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// string(11) "mysqli_stmt"
/// mysqli_stmt object is not fully initialized
/// ```
#[php_function]
fn mysqli_stmt_init(req: &mut Request, mysql: Link) -> Answer<OrFalse<Owner<Statement>>> {
    stmt::stmt_init(req, on(mysql))
}

/// `mysqli_stmt_insert_id(mysqli_stmt $statement): int|string`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_query($l, 'CREATE TEMPORARY TABLE si (id INT AUTO_INCREMENT PRIMARY KEY)');
/// $s = mysqli_prepare($l, 'INSERT INTO si VALUES (NULL)'); mysqli_stmt_execute($s); var_dump(mysqli_stmt_insert_id($s));
/// ```
/// ```expect mysql
/// int(1)
/// ```
#[php_function]
fn mysqli_stmt_insert_id(req: &mut Request, statement: Statement) -> Answer<IntOrString> {
    stmt::count(req, on_stmt(statement), Need::Open, |s| s.insert_id).map(AsIs::new)
}

/// `mysqli_stmt_more_results(mysqli_stmt $statement): bool`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// mysqli_stmt_execute($s); var_dump(mysqli_stmt_more_results($s));
/// ```
/// ```expect mysql
/// bool(false)
/// ```
#[php_function]
fn mysqli_stmt_more_results(req: &mut Request, statement: Statement) -> Answer<bool> {
    stmt::more_results(req, on_stmt(statement))
}

/// `mysqli_stmt_next_result(mysqli_stmt $statement): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// mysqli_stmt_execute($s); var_dump(mysqli_stmt_next_result($s));
/// ```
/// ```expect mysql
/// bool(false)
/// ```
#[php_function]
fn mysqli_stmt_next_result(req: &mut Request, statement: Statement) -> Answer<bool> {
    stmt::next_result(req, on_stmt(statement))
}

/// `mysqli_stmt_num_rows(mysqli_stmt $statement): int|string`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1 UNION SELECT 2');
/// mysqli_stmt_execute($s); var_dump(mysqli_stmt_num_rows($s)); mysqli_stmt_store_result($s); var_dump(mysqli_stmt_num_rows($s));
/// ```
/// ```expect mysql
/// int(0)
/// int(2)
/// ```
#[php_function]
fn mysqli_stmt_num_rows(req: &mut Request, statement: Statement) -> Answer<IntOrString> {
    stmt::count(req, on_stmt(statement), Need::Open, |s| s.num_rows).map(AsIs::new)
}

/// `mysqli_stmt_param_count(mysqli_stmt $statement): int`.
///
/// ```php
/// var_dump(mysqli_stmt_param_count(mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT ?, ?')));
/// ```
/// ```expect mysql
/// int(2)
/// ```
#[php_function]
fn mysqli_stmt_param_count(req: &mut Request, statement: Statement) -> Answer<i64> {
    Ok(i64::from(
        stmt::error_facts(req, on_stmt(statement), Need::Open)?.parameter_count,
    ))
}

/// `mysqli_stmt_prepare(mysqli_stmt $statement, string $query): bool`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $s = mysqli_stmt_init($l = mysqli_connect('127.0.0.1', 'root', '', 'test'));
/// var_dump(mysqli_stmt_prepare($s, 'SELEC'), mysqli_stmt_prepare($s, 'SELECT 1'), mysqli_stmt_execute($s));
/// ```
/// ```expect mysql
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn mysqli_stmt_prepare(req: &mut Request, statement: Statement, query: PhpString) -> Answer<bool> {
    stmt::stmt_prepare(req, on_stmt(statement), query)
}

/// `mysqli_stmt_reset(mysqli_stmt $statement): bool`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// var_dump(mysqli_stmt_reset($s));
/// ```
/// ```expect mysql
/// bool(true)
/// ```
#[php_function]
fn mysqli_stmt_reset(req: &mut Request, statement: Statement) -> Answer<bool> {
    stmt::reset(req, on_stmt(statement))
}

/// `mysqli_stmt_result_metadata(mysqli_stmt $statement): mysqli_result|false`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $m = mysqli_stmt_result_metadata(mysqli_prepare($l, 'SELECT 1 AS a'));
/// var_dump($m->field_count, $m->fetch_field()->name, $m->fetch_row(), mysqli_stmt_result_metadata(mysqli_prepare($l, 'DO 1')));
/// ```
/// ```expect mysql
/// int(1)
/// string(1) "a"
/// NULL
/// bool(false)
/// ```
#[php_function]
fn mysqli_stmt_result_metadata(
    req: &mut Request,
    statement: Statement,
) -> Answer<OrFalse<Owner<Res>>> {
    stmt::result_metadata(req, on_stmt(statement))
}

/// `mysqli_stmt_send_long_data(mysqli_stmt $statement, int $param_num, string $data): bool`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT ?');
/// $b = null; mysqli_stmt_bind_param($s, 'b', $b);
/// var_dump(mysqli_stmt_send_long_data($s, 0, 'ab'), mysqli_stmt_send_long_data($s, 0, 'cd'));
/// mysqli_stmt_execute($s); var_dump(mysqli_stmt_get_result($s)->fetch_row());
/// try { mysqli_stmt_send_long_data($s, -1, 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect mysql
/// bool(true)
/// bool(true)
/// array(1) {
///   [0]=>
///   string(4) "abcd"
/// }
/// mysqli_stmt_send_long_data(): Argument #2 ($param_num) must be greater than or equal to 0
/// ```
#[php_function]
fn mysqli_stmt_send_long_data(
    req: &mut Request,
    statement: Statement,
    param_num: i64,
    data: PhpString,
) -> Answer<bool> {
    stmt::send_long_data(req, on_stmt(statement), param_num, data)
}

/// `mysqli_stmt_store_result(mysqli_stmt $statement): bool`.
///
/// ```php
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT 1');
/// mysqli_stmt_execute($s); var_dump(mysqli_stmt_store_result($s), mysqli_stmt_num_rows($s));
/// ```
/// ```expect mysql
/// bool(true)
/// int(1)
/// ```
#[php_function]
fn mysqli_stmt_store_result(req: &mut Request, statement: Statement) -> Answer<bool> {
    stmt::store_result(req, on_stmt(statement))
}

/// `mysqli_stmt_sqlstate(mysqli_stmt $statement): string`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $s = mysqli_prepare($l = mysqli_connect('127.0.0.1', 'root', '', 'test'), 'SELECT ?');
/// var_dump(mysqli_stmt_sqlstate($s)); mysqli_stmt_execute($s); var_dump(mysqli_stmt_sqlstate($s));
/// ```
/// ```expect mysql
/// string(5) "00000"
/// string(5) "HY000"
/// ```
#[php_function]
fn mysqli_stmt_sqlstate(req: &mut Request, statement: Statement) -> Answer<Owner<PhpString>> {
    let state = stmt::error_facts(req, on_stmt(statement), Need::Open)?.sqlstate;
    PhpString::new(req, &state)
}

/// `mysqli_sqlstate(mysqli $mysql): string`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// var_dump(mysqli_sqlstate($l)); mysqli_query($l, 'SELECT nope'); var_dump(mysqli_sqlstate($l));
/// ```
/// ```expect mysql
/// string(5) "00000"
/// string(5) "42S22"
/// ```
#[php_function]
fn mysqli_sqlstate(req: &mut Request, mysql: Link) -> Answer<Owner<PhpString>> {
    let state = link::status(req, on(mysql))?.sqlstate;
    PhpString::new(req, &state)
}

/// `mysqli_ssl_set(mysqli $mysql, ?string $key, ?string $certificate, ?string $ca_certificate, ?string $ca_path, ?string $cipher_algos): true`.
///
/// ```php
/// var_dump(mysqli_ssl_set(mysqli_init(), null, null, '', null, null));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn mysqli_ssl_set(
    req: &mut Request,
    mysql: Link,
    key: Option<PhpString>,
    certificate: Option<PhpString>,
    ca_certificate: Option<PhpString>,
    ca_path: Option<PhpString>,
    cipher_algos: Option<PhpString>,
) -> Answer<True> {
    let parts = [key, certificate, ca_certificate, ca_path, cipher_algos];
    link::ssl_set(req, on(mysql), parts)
}

/// `mysqli_stat(mysqli $mysql): string|false`.
///
/// ```php
/// var_dump(str_starts_with(mysqli_stat(mysqli_connect('127.0.0.1', 'root', '', 'test')), 'Uptime: '));
/// ```
/// ```expect mysql
/// bool(true)
/// ```
#[php_function]
fn mysqli_stat(req: &mut Request, mysql: Link) -> Answer<OrFalse<Owner<PhpString>>> {
    link::stat(req, on(mysql))
}

/// `mysqli_store_result(mysqli $mysql, int $mode = 0): mysqli_result|false`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_real_query($l, 'SELECT 1'); var_dump(mysqli_store_result($l)->fetch_row(), mysqli_store_result($l));
/// mysqli_real_query($l, 'SELECT 2'); var_dump(mysqli_store_result($l, MYSQLI_STORE_RESULT)->num_rows);
/// ```
/// ```expect mysql
/// array(1) {
///   [0]=>
///   string(1) "1"
/// }
/// bool(false)
///
/// Deprecated: Passing the $mode parameter is deprecated since 8.4, as it has been ignored since 8.1 in %s on line 5
/// int(1)
/// ```
#[php_function]
fn mysqli_store_result(
    req: &mut Request,
    mysql: Link,
    #[php(default = 0)] mode: i64,
) -> Answer<OrFalse<Owner<Res>>> {
    let _ = mode;
    let given = req.frames().argument_count() == 2;
    link::store_result(req, on(mysql), given)
}

/// `mysqli_thread_id(mysqli $mysql): int`.
///
/// ```php
/// var_dump(mysqli_thread_id(mysqli_connect('127.0.0.1', 'root', '', 'test')) > 0);
/// ```
/// ```expect mysql
/// bool(true)
/// ```
#[php_function]
fn mysqli_thread_id(req: &mut Request, mysql: Link) -> Answer<i64> {
    Ok(i64::try_from(link::status(req, on(mysql))?.thread_id).unwrap_or(0))
}

/// `mysqli_thread_safe(): bool`: `true`, as `mysqlnd_thread_safe` answers
/// in every build.
///
/// ```php
/// var_dump(mysqli_thread_safe());
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn mysqli_thread_safe() -> bool {
    true
}

/// `mysqli_use_result(mysqli $mysql): mysqli_result|false`.
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_real_query($l, 'SELECT 1'); $r = mysqli_use_result($l); var_dump($r->type, $r->fetch_row(), mysqli_use_result($l));
/// ```
/// ```expect mysql
/// int(1)
/// array(1) {
///   [0]=>
///   string(1) "1"
/// }
/// bool(false)
/// ```
#[php_function]
fn mysqli_use_result(req: &mut Request, mysql: Link) -> Answer<OrFalse<Owner<Res>>> {
    link::take_rows(req, on(mysql), link::USE)
}

/// `mysqli_warning_count(mysqli $mysql): int`.
///
/// ```php
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_query($l, 'SELECT 1/0'); var_dump(mysqli_warning_count($l));
/// ```
/// ```expect mysql
/// int(1)
/// ```
#[php_function]
fn mysqli_warning_count(req: &mut Request, mysql: Link) -> Answer<i64> {
    Ok(i64::from(link::status(req, on(mysql))?.warning_count))
}

/// `mysqli_refresh(mysqli $mysql, int $flags): bool`, deprecated since 8.4.
///
/// ```php
/// var_dump(@mysqli_refresh(mysqli_connect('127.0.0.1', 'root', '', 'test'), MYSQLI_REFRESH_LOG));
/// ```
/// ```expect mysql
/// bool(false)
/// ```
#[php_function(deprecated(since = "8.4", message = "use FLUSH SQL statement instead"))]
fn mysqli_refresh(req: &mut Request, mysql: Link, flags: i64) -> Answer<bool> {
    link::refresh(req, on(mysql), flags)
}
