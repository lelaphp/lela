//! `mysqlnd_enum_n_def.h`: the protocol's numbers, each decoded once into
//! the type that carries it. A client bit or a status is a bit set php
//! tests bit by bit, so it stays one; a column's type, a connection's
//! state and a command are enums.

/// `CLIENT_*`, the client bits of the handshake.
pub(crate) mod client {
    pub(crate) const LONG_PASSWORD: u32 = 1;
    pub(crate) const LONG_FLAG: u32 = 4;
    pub(crate) const CONNECT_WITH_DB: u32 = 8;
    pub(crate) const COMPRESS: u32 = 32;
    pub(crate) const ODBC: u32 = 64;
    pub(crate) const LOCAL_FILES: u32 = 128;
    pub(crate) const PROTOCOL_41: u32 = 512;
    pub(crate) const SSL: u32 = 2048;
    pub(crate) const TRANSACTIONS: u32 = 8192;
    pub(crate) const SECURE_CONNECTION: u32 = 32768;
    pub(crate) const MULTI_RESULTS: u32 = 1 << 17;
    pub(crate) const PLUGIN_AUTH: u32 = 1 << 19;
    pub(crate) const CONNECT_ATTRS: u32 = 1 << 20;
    pub(crate) const CAN_HANDLE_EXPIRED_PASSWORDS: u32 = 1 << 22;
    /// mysqlnd's reuse of `CLIENT_ODBC`: TLS without checking the peer.
    pub(crate) const SSL_DONT_VERIFY_SERVER_CERT: u32 = ODBC;
    pub(crate) const SSL_VERIFY_SERVER_CERT: u32 = 1 << 30;
    /// `MYSQLND_CAPABILITIES`, what every connect asks for.
    pub(crate) const MYSQLND_CAPABILITIES: u32 = LONG_PASSWORD
        | LONG_FLAG
        | TRANSACTIONS
        | PROTOCOL_41
        | SECURE_CONNECTION
        | MULTI_RESULTS
        | LOCAL_FILES
        | PLUGIN_AUTH;
}

/// `SERVER_*`, the status bits an OK, an EOF and a greeting carry.
pub(crate) mod status {
    pub(crate) const MORE_RESULTS_EXISTS: u32 = 8;
    pub(crate) const QUERY_NO_GOOD_INDEX_USED: u32 = 16;
    pub(crate) const QUERY_NO_INDEX_USED: u32 = 32;
    pub(crate) const CURSOR_EXISTS: u32 = 64;
    pub(crate) const NO_BACKSLASH_ESCAPES: u32 = 512;
    pub(crate) const QUERY_WAS_SLOW: u32 = 2048;
    pub(crate) const PS_OUT_PARAMS: u32 = 4096;
}

/// A column's flags, `NOT_NULL_FLAG` and its kin.
pub mod column {
    pub const NOT_NULL: u32 = 1;
    pub const PRI_KEY: u32 = 2;
    pub const UNIQUE_KEY: u32 = 4;
    pub const MULTIPLE_KEY: u32 = 8;
    pub const BLOB: u32 = 16;
    pub const UNSIGNED: u32 = 32;
    pub const ZEROFILL: u32 = 64;
    pub const BINARY: u32 = 128;
    pub const ENUM: u32 = 256;
    pub const AUTO_INCREMENT: u32 = 512;
    pub const TIMESTAMP: u32 = 1024;
    pub const SET: u32 = 2048;
    pub const NO_DEFAULT: u32 = 4096;
    pub const ON_UPDATE_NOW: u32 = 8192;
    pub const PART_KEY: u32 = 16384;
    pub const GROUP: u32 = 32768;
    pub const NUM: u32 = 32768;
}

/// The client errors mysqlnd raises itself, `CR_*`.
pub(crate) mod cr {
    pub(crate) const UNKNOWN_ERROR: u32 = 2000;
    pub(crate) const CONNECTION_ERROR: u32 = 2002;
    pub(crate) const SERVER_GONE_ERROR: u32 = 2006;
    pub(crate) const OUT_OF_MEMORY: u32 = 2008;
    pub(crate) const SERVER_LOST: u32 = 2013;
    pub(crate) const COMMANDS_OUT_OF_SYNC: u32 = 2014;
    pub(crate) const CANT_FIND_CHARSET: u32 = 2019;
    pub(crate) const MALFORMED_PACKET: u32 = 2027;
    pub(crate) const NOT_IMPLEMENTED: u32 = 2054;
    pub(crate) const NO_PREPARE_STMT: u32 = 2030;
    pub(crate) const PARAMS_NOT_BOUND: u32 = 2031;
    pub(crate) const INVALID_PARAMETER_NO: u32 = 2034;
    pub(crate) const INVALID_BUFFER_USE: u32 = 2035;
    pub(crate) const LOAD_DATA_LOCAL_INFILE_REJECTED: u32 = 2068;
    pub(crate) const CLIENT_INTERACTION_TIMEOUT: u32 = 4031;
    /// `MYSQLND_EE_FILENOTFOUND`.
    pub(crate) const EE_FILENOTFOUND: u32 = 7890;
}

/// `UNKNOWN_SQLSTATE`.
pub(crate) const UNKNOWN_SQLSTATE: &[u8; 5] = b"HY000";
/// `MYSQLND_MAX_PACKET_SIZE`: one frame's largest payload.
pub(crate) const MAX_PACKET_SIZE: usize = 256 * 256 * 256 - 1;
/// `MYSQLND_ASSEMBLED_PACKET_MAX_SIZE`.
pub(crate) const ASSEMBLED_PACKET_MAX_SIZE: u32 = 3 * 1024 * 1024 * 1024;
/// `MYSQLND_MAX_ALLOWED_USER_LEN` and `MYSQLND_MAX_ALLOWED_DB_LEN`.
pub(crate) const MAX_ALLOWED_USER_LEN: usize = 252;
pub(crate) const MAX_ALLOWED_DB_LEN: usize = 1024;
/// `MYSQLND_NET_CMD_BUFFER_MIN_SIZE`.
pub(crate) const NET_CMD_BUFFER_MIN_SIZE: u32 = 4096;
/// `SCRAMBLE_LENGTH`.
pub(crate) const SCRAMBLE_LENGTH: usize = 20;
/// `MYSQLND_DEFAULT_AUTH_PROTOCOL`.
pub(crate) const DEFAULT_AUTH_PROTOCOL: &str = "mysql_native_password";
/// `MYSQLND_ERRMSG_SIZE`: an error text is cut at one byte less.
pub(crate) const ERRMSG_SIZE: usize = 512;
/// `MYSQLND_NULL_LENGTH`, a length-encoded `NULL`.
pub(crate) const NULL_LENGTH: u64 = u64::MAX;

/// `enum php_mysqlnd_server_command`, `COM_*`: the commands mysqlnd
/// sends, by php's numbers, with `mysqlnd_command_to_text`'s names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Com {
    Quit = 1,
    InitDb = 2,
    Query = 3,
    Refresh = 7,
    Statistics = 9,
    ProcessKill = 12,
    Debug = 13,
    Ping = 14,
    ChangeUser = 17,
    StmtPrepare = 22,
    StmtExecute = 23,
    StmtSendLongData = 24,
    StmtClose = 25,
    StmtReset = 26,
    SetOption = 27,
    StmtFetch = 28,
}

impl Com {
    /// `mysqlnd_command_to_text`.
    pub(crate) fn text(self) -> &'static str {
        match self {
            Com::Quit => "QUIT",
            Com::InitDb => "INIT_DB",
            Com::Query => "QUERY",
            Com::Refresh => "REFRESH",
            Com::Statistics => "STATISTICS",
            Com::ProcessKill => "PROCESS_KILL",
            Com::Debug => "DEBUG",
            Com::Ping => "PING",
            Com::ChangeUser => "CHANGE_USER",
            Com::StmtPrepare => "STMT_PREPARE",
            Com::StmtExecute => "STMT_EXECUTE",
            Com::StmtSendLongData => "STMT_SEND_LONG_DATA",
            Com::StmtClose => "STMT_CLOSE",
            Com::StmtReset => "STMT_RESET",
            Com::SetOption => "SET_OPTION",
            Com::StmtFetch => "STMT_FETCH",
        }
    }
}

/// `enum mysqlnd_connection_state`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, php_ext::abi::EngineState,
)]
pub(crate) enum ConnState {
    #[default]
    Alloced,
    Ready,
    QuerySent,
    SendingLoadData,
    FetchingData,
    NextResultPending,
    QuitSent,
}

/// `enum mysqlnd_query_type`: what the last query answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, php_ext::abi::EngineState)]
pub(crate) enum QueryType {
    #[default]
    Upsert,
    Select,
    LoadLocal,
}

/// `enum mysqlnd_field_types`, decoded from the column packet's byte;
/// a byte that names none is `Other` and refused where php refuses it
/// ("Unknown type %u sent by the server").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Decimal,
    Tiny,
    Short,
    Long,
    Float,
    Double,
    Null,
    Timestamp,
    LongLong,
    Int24,
    Date,
    Time,
    DateTime,
    Year,
    NewDate,
    VarChar,
    Bit,
    Vector,
    Json,
    NewDecimal,
    Enum,
    Set,
    TinyBlob,
    MediumBlob,
    LongBlob,
    Blob,
    VarString,
    String,
    Geometry,
    Other(u8),
}

/// How php's `mysqlnd_ps_fetch_functions` row for a type turns its bytes
/// into a value, the half both protocols read: the PHP type it makes
/// and the length of its packed form, `0` for a length-encoded one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhpType {
    Null,
    Long,
    Double,
    String,
}

impl FieldType {
    /// The type of the column packet's byte.
    pub fn of(byte: u8) -> FieldType {
        match byte {
            0 => FieldType::Decimal,
            1 => FieldType::Tiny,
            2 => FieldType::Short,
            3 => FieldType::Long,
            4 => FieldType::Float,
            5 => FieldType::Double,
            6 => FieldType::Null,
            7 => FieldType::Timestamp,
            8 => FieldType::LongLong,
            9 => FieldType::Int24,
            10 => FieldType::Date,
            11 => FieldType::Time,
            12 => FieldType::DateTime,
            13 => FieldType::Year,
            14 => FieldType::NewDate,
            15 => FieldType::VarChar,
            16 => FieldType::Bit,
            242 => FieldType::Vector,
            245 => FieldType::Json,
            246 => FieldType::NewDecimal,
            247 => FieldType::Enum,
            248 => FieldType::Set,
            249 => FieldType::TinyBlob,
            250 => FieldType::MediumBlob,
            251 => FieldType::LongBlob,
            252 => FieldType::Blob,
            253 => FieldType::VarString,
            254 => FieldType::String,
            255 => FieldType::Geometry,
            other => FieldType::Other(other),
        }
    }

    /// The protocol's byte for the type.
    pub fn byte(self) -> u8 {
        match self {
            FieldType::Decimal => 0,
            FieldType::Tiny => 1,
            FieldType::Short => 2,
            FieldType::Long => 3,
            FieldType::Float => 4,
            FieldType::Double => 5,
            FieldType::Null => 6,
            FieldType::Timestamp => 7,
            FieldType::LongLong => 8,
            FieldType::Int24 => 9,
            FieldType::Date => 10,
            FieldType::Time => 11,
            FieldType::DateTime => 12,
            FieldType::Year => 13,
            FieldType::NewDate => 14,
            FieldType::VarChar => 15,
            FieldType::Bit => 16,
            FieldType::Vector => 242,
            FieldType::Json => 245,
            FieldType::NewDecimal => 246,
            FieldType::Enum => 247,
            FieldType::Set => 248,
            FieldType::TinyBlob => 249,
            FieldType::MediumBlob => 250,
            FieldType::LongBlob => 251,
            FieldType::Blob => 252,
            FieldType::VarString => 253,
            FieldType::String => 254,
            FieldType::Geometry => 255,
            FieldType::Other(byte) => byte,
        }
    }

    /// `mysqlnd_ps_fetch_functions[type]`'s `php_type` and `pack_len`, or
    /// `None` for a type that row has no function for.
    pub(crate) fn fetch(self) -> Option<(PhpType, usize)> {
        Some(match self {
            FieldType::Null => (PhpType::Null, 0),
            FieldType::Tiny => (PhpType::Long, 1),
            FieldType::Short | FieldType::Year => (PhpType::Long, 2),
            FieldType::Int24 | FieldType::Long => (PhpType::Long, 4),
            FieldType::LongLong | FieldType::Bit => (PhpType::Long, 8),
            FieldType::Float => (PhpType::Double, 4),
            FieldType::Double => (PhpType::Double, 8),
            FieldType::Other(_) => return None,
            _ => (PhpType::String, 0),
        })
    }

    /// `IS_NUM(t)`.
    pub fn is_num(self) -> bool {
        self.byte() <= FieldType::Int24.byte()
            || matches!(self, FieldType::Year | FieldType::NewDecimal)
    }
}

/// `enum mysqlnd_stmt_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StmtState {
    Initted,
    Prepared,
    Executed,
    WaitingUseOrStore,
    UseOrStoreCalled,
    UserFetching,
}

/// `enum myslqnd_cursor_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CursorType {
    #[default]
    NoCursor,
    ReadOnly,
}

/// `enum mysqlnd_res_type`: how a `MYSQLND_RES`'s rows are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResType {
    Normal,
    PsBuffered,
    PsUnbuffered,
}

/// `enum mysqlnd_connection_close_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseType {
    Explicit,
    Implicit,
    Disconnected,
}

/// `TRANS_START_*`, the `START TRANSACTION` characteristics.
pub(crate) mod trans_start {
    pub(crate) const WITH_CONSISTENT_SNAPSHOT: u32 = 1;
    pub(crate) const READ_WRITE: u32 = 2;
    pub(crate) const READ_ONLY: u32 = 4;
}

/// `TRANS_COR_*`, the `COMMIT` and `ROLLBACK` options.
pub(crate) mod trans_cor {
    pub(crate) const AND_CHAIN: u32 = 1;
    pub(crate) const AND_NO_CHAIN: u32 = 2;
    pub(crate) const RELEASE: u32 = 4;
    pub(crate) const NO_RELEASE: u32 = 8;
}

/// `MYSQLND_FETCH_ASSOC`, `MYSQLND_FETCH_NUM` and the two together.
pub(crate) mod fetch {
    pub(crate) const ASSOC: u32 = 1;
    pub(crate) const NUM: u32 = 2;
}

/// `enum mysqlnd_server_option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerOption {
    MultiStatementsOn,
    MultiStatementsOff,
}
