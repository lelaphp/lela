//! `pdo_sqlstate.c`, `pdo_raise_impl_error`, `pdo_handle_error` and
//! `pdo_throw_exception`: the SQLSTATE descriptions, and what PDO does
//! with an error under the connection's error handling, the SQLSTATE recorded on
//! the connection or the statement it was raised on, then a warning, a
//! `PDOException`, or nothing.

use php_ext::raising::{Level, Thrown, docref, new_throwable, throw_value};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, Value};

use crate::connector::{Cause, Failure, NativeError, OnError, SqlError, SqlState};
use crate::stmt::Stmt;
use crate::throwables::Throwables;

/// `err_initializer`, each state's five characters and its description,
/// `|` between rows, in `pdo_sqlstate.c`'s order.
const STATES: &str = concat!(
    "00000No error|01000Warning|01001Cursor operation conflict|01002Disconnect error|",
    "01003NULL value eliminated in set function|01004String data, right truncated|",
    "01006Privilege not revoked|01007Privilege not granted|01008Implicit zero bit padding|",
    "0100CDynamic result sets returned|01P01Deprecated feature|",
    "01S00Invalid connection string attribute|01S01Error in row|01S02Option value changed|",
    "01S06Attempt to fetch before the result set returned the first rowset|",
    "01S07Fractional truncation|01S08Error saving File DSN|01S09Invalid keyword|02000No data|",
    "02001No additional dynamic result sets returned|03000Sql statement not yet complete|",
    "07002COUNT field incorrect|07005Prepared statement not a cursor-specification|",
    "07006Restricted data type attribute violation|07009Invalid descriptor index|",
    "07S01Invalid use of default parameter|08000Connection exception|",
    "08001Client unable to establish connection|08002Connection name in use|",
    "08003Connection does not exist|08004Server rejected the connection|",
    "08006Connection failure|08007Connection failure during transaction|",
    "08S01Communication link failure|09000Triggered action exception|",
    "0A000Feature not supported|0B000Invalid transaction initiation|0F000Locator exception|",
    "0F001Invalid locator specification|0L000Invalid grantor|0LP01Invalid grant operation|",
    "0P000Invalid role specification|21000Cardinality violation|",
    "21S01Insert value list does not match column list|",
    "21S02Degree of derived table does not match column list|22000Data exception|",
    "22001String data, right truncated|22002Indicator variable required but not supplied|",
    "22003Numeric value out of range|22004Null value not allowed|22005Error in assignment|",
    "22007Invalid datetime format|22008Datetime field overflow|",
    "22009Invalid time zone displacement value|2200BEscape character conflict|",
    "2200CInvalid use of escape character|2200DInvalid escape octet|",
    "2200FZero length character string|2200GMost specific type mismatch|",
    "22010Invalid indicator parameter value|22011Substring error|22012Division by zero|",
    "22015Interval field overflow|22018Invalid character value for cast specification|",
    "22019Invalid escape character|2201BInvalid regular expression|",
    "2201EInvalid argument for logarithm|2201FInvalid argument for power function|",
    "2201GInvalid argument for width bucket function|22020Invalid limit value|",
    "22021Character not in repertoire|22022Indicator overflow|22023Invalid parameter value|",
    "22024Unterminated c string|22025Invalid escape sequence|",
    "22026String data, length mismatch|22027Trim error|2202EArray subscript error|",
    "22P01Floating point exception|22P02Invalid text representation|",
    "22P03Invalid binary representation|22P04Bad copy file format|",
    "22P05Untranslatable character|23000Integrity constraint violation|",
    "23001Restrict violation|23502Not null violation|23503Foreign key violation|",
    "23505Unique violation|23514Check violation|24000Invalid cursor state|",
    "25000Invalid transaction state|25001Active sql transaction|",
    "25002Branch transaction already active|",
    "25003Inappropriate access mode for branch transaction|",
    "25004Inappropriate isolation level for branch transaction|",
    "25005No active sql transaction for branch transaction|25006Read only sql transaction|",
    "25007Schema and data statement mixing not supported|",
    "25008Held cursor requires same isolation level|25P01No active sql transaction|",
    "25P02In failed sql transaction|25S01Transaction state|25S02Transaction is still active|",
    "25S03Transaction is rolled back|26000Invalid sql statement name|",
    "27000Triggered data change violation|28000Invalid authorization specification|",
    "2B000Dependent privilege descriptors still exist|2BP01Dependent objects still exist|",
    "2D000Invalid transaction termination|2F000Sql routine exception|",
    "2F002Modifying sql data not permitted|2F003Prohibited sql statement attempted|",
    "2F004Reading sql data not permitted|2F005Function executed no return statement|",
    "34000Invalid cursor name|38000External routine exception|",
    "38001Containing sql not permitted|38002Modifying sql data not permitted|",
    "38003Prohibited sql statement attempted|38004Reading sql data not permitted|",
    "39000External routine invocation exception|39001Invalid sqlstate returned|",
    "39004Null value not allowed|39P01Trigger protocol violated|39P02Srf protocol violated|",
    "3B000Savepoint exception|3B001Invalid savepoint specification|",
    "3C000Duplicate cursor name|3D000Invalid catalog name|3F000Invalid schema name|",
    "40000Transaction rollback|40001Serialization failure|",
    "40002Transaction integrity constraint violation|40003Statement completion unknown|",
    "40P01Deadlock detected|42000Syntax error or access violation|",
    "42501Insufficient privilege|42601Syntax error|42602Invalid name|",
    "42611Invalid column definition|42622Name too long|42701Duplicate column|",
    "42702Ambiguous column|42703Undefined column|42704Undefined object|42710Duplicate object|",
    "42712Duplicate alias|42723Duplicate function|42725Ambiguous function|",
    "42803Grouping error|42804Datatype mismatch|42809Wrong object type|",
    "42830Invalid foreign key|42846Cannot coerce|42883Undefined function|42939Reserved name|",
    "42P01Undefined table|42P02Undefined parameter|42P03Duplicate cursor|",
    "42P04Duplicate database|42P05Duplicate prepared statement|42P06Duplicate schema|",
    "42P07Duplicate table|42P08Ambiguous parameter|42P09Ambiguous alias|",
    "42P10Invalid column reference|42P11Invalid cursor definition|",
    "42P12Invalid database definition|42P13Invalid function definition|",
    "42P14Invalid prepared statement definition|42P15Invalid schema definition|",
    "42P16Invalid table definition|42P17Invalid object definition|",
    "42P18Indeterminate datatype|42S01Base table or view already exists|",
    "42S02Base table or view not found|42S11Index already exists|42S12Index not found|",
    "42S21Column already exists|42S22Column not found|44000WITH CHECK OPTION violation|",
    "53000Insufficient resources|53100Disk full|53200Out of memory|53300Too many connections|",
    "54000Program limit exceeded|54001Statement too complex|54011Too many columns|",
    "54023Too many arguments|55000Object not in prerequisite state|55006Object in use|",
    "55P02Cant change runtime param|55P03Lock not available|57000Operator intervention|",
    "57014Query canceled|57P01Admin shutdown|57P02Crash shutdown|57P03Cannot connect now|",
    "58030Io error|58P01Undefined file|58P02Duplicate file|F0000Config file error|",
    "F0001Lock file exists|HY000General error|HY001Memory allocation error|",
    "HY003Invalid application buffer type|HY004Invalid SQL data type|",
    "HY007Associated statement is not prepared|HY008Operation canceled|",
    "HY009Invalid use of null pointer|HY010Function sequence error|",
    "HY011Attribute cannot be set now|HY012Invalid transaction operation code|",
    "HY013Memory management error|HY014Limit on the number of handles exceeded|",
    "HY015No cursor name available|HY016Cannot modify an implementation row descriptor|",
    "HY017Invalid use of an automatically allocated descriptor handle|",
    "HY018Server declined cancel request|",
    "HY019Non-character and non-binary data sent in pieces|",
    "HY020Attempt to concatenate a null value|HY021Inconsistent descriptor information|",
    "HY024Invalid attribute value|HY090Invalid string or buffer length|",
    "HY091Invalid descriptor field identifier|HY092Invalid attribute/option identifier|",
    "HY093Invalid parameter number|HY095Function type out of range|",
    "HY096Invalid information type|HY097Column type out of range|",
    "HY098Scope type out of range|HY099Nullable type out of range|",
    "HY100Uniqueness option type out of range|HY101Accuracy option type out of range|",
    "HY103Invalid retrieval code|HY104Invalid precision or scale value|",
    "HY105Invalid parameter type|HY106Fetch type out of range|HY107Row value out of range|",
    "HY109Invalid cursor position|HY110Invalid driver completion|HY111Invalid bookmark value|",
    "HYC00Optional feature not implemented|HYT00Timeout expired|",
    "HYT01Connection timeout expired|IM001Driver does not support this function|",
    "IM002Data source name not found and no default driver specified|",
    "IM003Specified driver could not be loaded|",
    "IM004Driver's SQLAllocHandle on SQL_HANDLE_ENV failed|",
    "IM005Driver's SQLAllocHandle on SQL_HANDLE_DBC failed|",
    "IM006Driver's SQLSetConnectAttr failed|",
    "IM007No data source or driver specified; dialog prohibited|IM008Dialog failed|",
    "IM009Unable to load translation DLL|IM010Data source name too long|",
    "IM011Driver name too long|IM012DRIVER keyword syntax error|IM013Trace file error|",
    "IM014Invalid name of File DSN|IM015Corrupt file data source|P0000Plpgsql error|",
    "P0001Raise exception|XX000Internal error|XX001Data corrupted|XX002Index corrupted|",
);

/// `pdo_sqlstate_state_to_description`.
fn description(state: SqlState) -> Option<&'static str> {
    STATES
        .split('|')
        .find(|row| row.as_bytes().get(..5) == Some(state.0.as_slice()))
        .and_then(|row| row.get(5..))
}

/// Where an error is recorded: `dbh->error_code`, or the statement's
/// `stmt->error_code`.
#[derive(Clone, Copy)]
pub(crate) enum On {
    Dbh,
    Stmt(PhpObject),
}

/// The SQLSTATE `on` holds.
pub(super) fn state(req: &mut Request, dbh: PhpObject, on: On) -> Option<SqlState> {
    match on {
        On::Dbh => crate::dbh::state(req, dbh).and_then(|h| h.error_code),
        On::Stmt(stmt) => stmt.native::<Stmt>(req).and_then(|s| s.error_code),
    }
}

/// `memcpy(*pdo_err, sqlstate, ...)`.
pub(super) fn set_state(req: &mut Request, dbh: PhpObject, on: On, state: SqlState) {
    match on {
        On::Dbh => {
            if let Some(h) = crate::dbh::state(req, dbh) {
                h.error_code = Some(state);
            }
        }
        On::Stmt(stmt) => {
            if let Some(s) = stmt.native_mut::<Stmt>(req) {
                s.error_code = Some(state);
            }
        }
    }
}

fn error_mode(req: &mut Request, dbh: PhpObject) -> OnError {
    crate::dbh::state(req, dbh).map_or(OnError::Throw, |h| h.error_mode)
}

/// A `PDOException` with `message`, `code` and `errorInfo` written as
/// `zend_update_property` writes them, thrown.
fn throw_pdo(
    req: &mut Request,
    message: &str,
    code: Owner<Value>,
    info: Option<Owner<PhpArray>>,
) -> Thrown {
    let made = Throwables::of(req).and_then(|ce| new_throwable(req, ce.pdo_exception, message));
    let value = match made {
        Ok(value) => value,
        Err(thrown) => {
            code.give_up(req);
            if let Some(info) = info {
                info.give_up(req);
            }
            return thrown;
        }
    };
    let Some(object) = value.as_object() else {
        code.give_up(req);
        if let Some(info) = info {
            info.give_up(req);
        }
        let thrown = throw_value(req, &value);
        value.give_up(req);
        return thrown;
    };
    let written = match php_ext::call::write_prop(req, &object, b"code", code) {
        Ok(()) => match info {
            Some(info) => php_ext::call::write_prop(req, &object, b"errorInfo", info.into_value()),
            None => Ok(()),
        },
        Err(thrown) => {
            if let Some(info) = info {
                info.give_up(req);
            }
            Err(thrown)
        }
    };
    if let Err(thrown) = written {
        value.give_up(req);
        return thrown;
    }
    let thrown = throw_value(req, &value);
    value.give_up(req);
    thrown
}

/// `pdo_raise_impl_error(dbh, stmt, sqlstate, supp)`: the state recorded
/// on `on`, and `SQLSTATE[s]: description[: supp]` raised as a warning
/// or thrown, whatever else is pending, as php throws it.
pub(super) fn raise_impl(
    req: &mut Request,
    dbh: PhpObject,
    on: On,
    state: SqlState,
    supp: Option<&str>,
) -> Result<(), Thrown> {
    set_state(req, dbh, on, state);
    let description = description(state).unwrap_or("<<Unknown error>>");
    let message = match supp {
        Some(supp) => format!("SQLSTATE[{}]: {description}: {supp}", state.as_str()),
        None => format!("SQLSTATE[{}]: {description}", state.as_str()),
    };
    if error_mode(req, dbh) != OnError::Throw {
        docref(req, Level::Warning, &message);
        return Ok(());
    }
    let code = crate::string(req, state.as_str().as_bytes())?;
    let mut info = PhpArray::new(req)?;
    let text = crate::string(req, state.as_str().as_bytes())?;
    info.append(req, text)?;
    info.append(req, Owner::int(0))?;
    Err(throw_pdo(req, &message, code, Some(info)))
}

/// `dbh->methods->fetch_err(dbh, stmt, info)`: the native code and text
/// the connector recorded, read from the connector itself, the statement
/// handed to it for an error on one.
fn fetch_err(req: &mut Request, dbh: PhpObject, on: On) -> Result<Option<NativeError>, Thrown> {
    crate::dbh::with_connection(req, dbh, |req, conn| match on {
        On::Dbh => conn.fetch_err(req, None),
        On::Stmt(stmt) => {
            crate::stmt::with_statement(req, stmt, |req, statement| conn.fetch_err(req, statement))?
        }
    })
}

/// `fetch_err` over `info`: the native code and text appended where the
/// connector recorded some.
pub(super) fn fetch_error(
    req: &mut Request,
    dbh: PhpObject,
    on: On,
    info: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    match fetch_err(req, dbh, on)? {
        Some(native) => append_native(req, info, native),
        None => Ok(()),
    }
}

/// `add_next_index_long(info, einfo->errcode)` and
/// `add_next_index_string(info, einfo->errmsg)`.
fn append_native(
    req: &mut Request,
    info: &mut Owner<PhpArray>,
    native: NativeError,
) -> Result<(), Thrown> {
    if let Err(thrown) = info.append(req, Owner::int(native.code)) {
        native.message.give_up(req);
        return Err(thrown);
    }
    info.append(req, native.message.into_value())
}

/// `pdo_handle_error(dbh, stmt)`: nothing under `ERRMODE_SILENT`;
/// otherwise the state on `on`, its description and what the connector
/// recorded as `SQLSTATE[s]: description: code text`, a warning, or a
/// `PDOException` carrying the state as its code and the info as its
/// `errorInfo` where no throwable is `pending`.
pub(super) fn handle(
    req: &mut Request,
    dbh: PhpObject,
    on: On,
    pending: bool,
) -> Result<(), Thrown> {
    let mode = error_mode(req, dbh);
    if mode == OnError::Silent {
        return Ok(());
    }
    let state = state(req, dbh, on).unwrap_or(SqlState([0; 5]));
    let description = description(state).unwrap_or("<<Unknown error>>");
    let mut info = PhpArray::new(req)?;
    let text = crate::string(req, state.as_str().as_bytes())?;
    info.append(req, text)?;
    let s = state.as_str();
    let message = match fetch_err(req, dbh, on)? {
        Some(native) => {
            let supp = String::from_utf8_lossy(native.message.as_bytes()).into_owned();
            let message = format!("SQLSTATE[{s}]: {description}: {} {supp}", native.code);
            append_native(req, &mut info, native)?;
            message
        }
        None => format!("SQLSTATE[{s}]: {description}"),
    };
    if mode == OnError::Warn {
        docref(req, Level::Warning, &message);
        info.give_up(req);
        return Ok(());
    }
    if pending {
        info.give_up(req);
        return Ok(());
    }
    let code = crate::string(req, s.as_bytes())?;
    Err(throw_pdo(req, &message, code, Some(info)))
}

/// What a connector's failure leaves, `PDO_HANDLE_DBH_ERR` or
/// `PDO_HANDLE_STMT_ERR` after the connector's slot: the cause recorded on
/// `on`, handled under the error handling when it left a state other than
/// `00000`, and the pending throwable let through.
pub(super) fn settle(
    req: &mut Request,
    dbh: PhpObject,
    on: On,
    failure: Failure,
) -> Result<(), Thrown> {
    let pending = failure.thrown.is_some();
    record(req, dbh, on, failure.cause)?;
    if state(req, dbh, on) != Some(SqlState::NONE) {
        let handled = handle(req, dbh, on, pending);
        if let Some(thrown) = failure.thrown {
            return Err(thrown);
        }
        handled?;
    }
    match failure.thrown {
        Some(thrown) => Err(thrown),
        None => Ok(()),
    }
}

/// A cause written where php's connector writes it: an error's state on
/// `on`, its native code and text being the connector's own; an
/// implementation error raised as the connector raises it.
pub(super) fn record(
    req: &mut Request,
    dbh: PhpObject,
    on: On,
    cause: Cause,
) -> Result<(), Thrown> {
    match cause {
        Cause::Error(state) | Cause::Refused(SqlError { state, .. }) => {
            set_state(req, dbh, on, state);
            Ok(())
        }
        Cause::Impl { state, message } => raise_impl(req, dbh, on, state, Some(&message)),
        Cause::Silent | Cause::Unsupported => Ok(()),
    }
}

/// `pdo_throw_exception`: a connector's error before the connection has its
/// methods, `SQLSTATE[s] [code] text` with the native code as the
/// throwable's code.
pub(super) fn throw_connect(req: &mut Request, failure: Failure) -> Thrown {
    if let Some(thrown) = failure.thrown {
        return thrown;
    }
    let Cause::Refused(error) = failure.cause else {
        let ce = match Throwables::of(req) {
            Ok(ce) => ce,
            Err(fatal) => return fatal,
        };
        return php_ext::raising::throw(req, ce.pdo_exception, "Constructor failed");
    };
    let text = String::from_utf8_lossy(&error.message).into_owned();
    let message = format!("SQLSTATE[{}] [{}] {text}", error.state.as_str(), error.code);
    let info = (|| {
        let mut info = PhpArray::new(req)?;
        let text = crate::string(req, error.state.as_str().as_bytes())?;
        info.append(req, text)?;
        info.append(req, Owner::int(error.code))?;
        let text = crate::string(req, &error.message)?;
        info.append(req, text)?;
        Ok::<Owner<PhpArray>, Thrown>(info)
    })();
    match info {
        Ok(info) => {
            let code = Owner::int(error.code);
            throw_pdo(req, &message, code, Some(info))
        }
        Err(thrown) => thrown,
    }
}
