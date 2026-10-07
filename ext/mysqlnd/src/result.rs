//! `mysqlnd_result.c` and `mysqlnd_result_meta.c`: a set of rows, its
//! columns' descriptions, and its rows, held whole (`store_result`) or read
//! one at a time off the connection (`use_result`), each row kept as the
//! bytes the server sent and decoded into values when it is fetched, as
//! php's row buffers are.

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString, Value};

use crate::conn::ConnData;
use crate::consts::{ConnState, FieldType, NULL_LENGTH, PhpType, ResType, column, cr, status};
use crate::error::{ErrorInfo, OUT_OF_SYNC};
use crate::stats::Stat;
use crate::wire::{Body, EODATA_MARKER, ERROR_MARKER, PacketError, Prot};

/// `MYSQLND_FIELD`: one column as the server described it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Field {
    pub name: Vec<u8>,
    pub org_name: Vec<u8>,
    pub table: Vec<u8>,
    pub org_table: Vec<u8>,
    pub db: Vec<u8>,
    pub catalog: Vec<u8>,
    pub def: Option<Vec<u8>>,
    /// `length`, the column's width.
    pub length: u64,
    pub flags: u32,
    pub decimals: u32,
    pub charsetnr: u32,
    /// The type's byte, [`Field::field_type`] decoded.
    pub type_byte: u8,
    /// `is_numeric` and `num_key`: the name as an integer key where php's
    /// `ZEND_HANDLE_NUMERIC` reads it as one.
    pub num_key: Option<i64>,
}

impl Field {
    /// The column's type.
    #[must_use]
    pub fn field_type(&self) -> FieldType {
        FieldType::of(self.type_byte)
    }
}

/// How a `MYSQLND_RES`'s rows are held.
#[derive(Debug)]
enum Rows {
    /// Column definitions read, no rows asked for yet.
    None,
    /// `MYSQLND_RES_BUFFERED`.
    Buffered { rows: Vec<Vec<u8>>, current: usize },
    /// `MYSQLND_RES_UNBUFFERED`.
    Unbuffered { eof: bool, row_count: u64 },
}

/// `MYSQLND_RES`.
#[derive(Debug)]
pub struct Res {
    pub(crate) field_count: u32,
    pub(crate) fields: Vec<Field>,
    /// `current_field`, the cursor of `fetch_field`.
    current_field: usize,
    pub(crate) kind: ResType,
    rows: Rows,
    /// Whether the rows are the binary protocol's, a statement's.
    binary: bool,
    /// `lengths`, of the last row fetched from text rows.
    lengths: Vec<u64>,
    /// The statement whose server-side cursor a statement's unbuffered
    /// rows are read from, row by row with `COM_STMT_FETCH`
    /// (`mysqlnd_fetch_stmt_row_cursor` in place of the unbuffered
    /// `fetch_row`).
    pub(crate) cursor: Option<u32>,
    /// The warnings and status the last cursor fetch read, which the
    /// statement takes into its own.
    pub(crate) cursor_status: (u32, u32),
}

/// One row fetched: its values in column order, one owner of each.
pub type Row = Vec<Owner<Value>>;

impl Res {
    /// `mysqlnd_result_init`.
    pub(crate) fn new(field_count: u32) -> Res {
        Res {
            field_count,
            fields: Vec::new(),
            current_field: 0,
            kind: ResType::Normal,
            rows: Rows::None,
            binary: false,
            lengths: Vec::new(),
            cursor: None,
            cursor_status: (0, 0),
        }
    }

    /// `result->unbuf` made with `eof_reached` set: rows that are only
    /// column definitions, as `mysqlnd_stmt_result_metadata` makes them.
    pub(crate) fn metadata_only(&mut self) {
        self.rows = Rows::Unbuffered {
            eof: true,
            row_count: 0,
        };
        self.binary = true;
    }

    /// `mysqlnd_res::free_result_buffers`: the rows and the column definitions
    /// dropped, the column count kept.
    pub(crate) fn free_buffers(&mut self) {
        self.rows = Rows::None;
        self.fields.clear();
        self.current_field = 0;
        self.cursor = None;
    }

    /// `mysqlnd_fetch_stmt_row_cursor`: one row asked of the statement's
    /// cursor and read, with the EOF that follows it; the warnings and
    /// status of the last packet kept on the connection and for the
    /// statement.
    fn fetch_cursor(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        stmt_id: u32,
        decode: bool,
    ) -> Result<Result<Option<Row>, ()>, Thrown> {
        conn.error.reset();
        let mut payload = stmt_id.to_le_bytes().to_vec();
        payload.extend_from_slice(&1u32.to_le_bytes());
        if !conn.send_command(req, crate::consts::Com::StmtFetch, &payload, false)? {
            return Ok(Err(()));
        }
        let native = conn.options.int_and_float_native;
        let (mut row, mut ok, mut last) = (None, true, (0, 0));
        match read_row(req, conn, true) {
            RowPacket::Row(bytes) => {
                if decode {
                    match self.decode(req, conn, &bytes, native)? {
                        Ok(values) => row = Some(values),
                        Err(()) => return Ok(Err(())),
                    }
                }
                match read_row(req, conn, true) {
                    RowPacket::Eof {
                        warning_count,
                        server_status,
                    } => last = (warning_count, server_status),
                    RowPacket::Row(_) => {}
                    RowPacket::Error(_) | RowPacket::Fail => ok = false,
                }
                conn.stats.inc(Stat::RowsFetchedFromClientPsCursor);
                if let Rows::Unbuffered { row_count, .. } = &mut self.rows {
                    *row_count += 1;
                }
                if row.is_none() {
                    row = Some(Vec::new());
                }
            }
            RowPacket::Eof {
                warning_count,
                server_status,
            } => {
                last = (warning_count, server_status);
                if let Rows::Unbuffered { eof, .. } = &mut self.rows {
                    *eof = true;
                }
            }
            RowPacket::Error(_) | RowPacket::Fail => ok = false,
        }
        conn.upsert.warning_count = last.0;
        conn.upsert.server_status = last.1;
        self.cursor_status = last;
        if !ok {
            if let Some(row) = row {
                for value in row {
                    value.give_up(req);
                }
            }
            return Ok(Err(()));
        }
        Ok(Ok(row))
    }

    /// The columns' descriptions.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// `mysqlnd_res::num_fields`.
    #[must_use]
    pub fn num_fields(&self) -> u32 {
        self.field_count
    }

    /// `mysqlnd_res::read_result_metadata` and `mysqlnd_res_meta::read_metadata`:
    /// one field packet per column.
    pub(crate) fn read_metadata(&mut self, req: &mut Request, conn: &mut ConnData) -> bool {
        self.fields.clear();
        self.current_field = 0;
        for _ in 0..self.field_count {
            let Some(body) = conn.read_packet(req, Prot::RsetField) else {
                self.fields.clear();
                return false;
            };
            match read_field(req, &body) {
                FieldPacket::Field(field) => {
                    if field.field_type().fetch().is_none() {
                        docref(
                            req,
                            Level::Warning,
                            &format!(
                                "Unknown type {} sent by the server. Please send a report to the developers",
                                field.type_byte
                            ),
                        );
                        self.fields.clear();
                        return false;
                    }
                    self.fields.push(field);
                }
                FieldPacket::Error(e) => {
                    e.set_on(&mut conn.error);
                    self.fields.clear();
                    return false;
                }
                FieldPacket::Fail => {
                    self.fields.clear();
                    return false;
                }
            }
        }
        true
    }

    /// `mysqlnd_res::use_result`: rows to be read one at a time; a
    /// statement's are the binary protocol's.
    pub(crate) fn use_unbuffered(&mut self, ps: bool) {
        self.kind = if ps {
            ResType::PsUnbuffered
        } else {
            ResType::Normal
        };
        self.binary = ps;
        self.rows = Rows::Unbuffered {
            eof: false,
            row_count: 0,
        };
        self.lengths = Vec::new();
    }

    /// `mysqlnd_res::store_result` with `store_result_fetch_data`: every
    /// row read and held; the status of the last EOF taken; the affected
    /// rows the row count, as libmysql documents.
    pub(crate) fn store(&mut self, req: &mut Request, conn: &mut ConnData, ps: bool) -> bool {
        self.kind = if ps {
            ResType::PsBuffered
        } else {
            ResType::Normal
        };
        self.binary = ps;
        conn.state = ConnState::FetchingData;
        let mut rows = Vec::new();
        let mut error = ErrorInfo::default();
        let mut failed = false;
        loop {
            match read_row(req, conn, ps) {
                RowPacket::Row(row) => rows.push(row),
                RowPacket::Eof {
                    warning_count,
                    server_status,
                } => {
                    conn.upsert.reset();
                    conn.upsert.warning_count = warning_count;
                    conn.upsert.server_status = server_status;
                    break;
                }
                RowPacket::Error(e) => {
                    e.set_on(&mut error);
                    failed = true;
                    break;
                }
                RowPacket::Fail => {
                    error.copy_from(&conn.error);
                    failed = true;
                    break;
                }
            }
        }
        let count = rows.len() as u64;
        conn.stats.add(
            if ps {
                Stat::RowsBufferedFromClientPs
            } else {
                Stat::RowsBufferedFromClientNormal
            },
            count,
        );
        if failed {
            conn.upsert.server_status &= !status::MORE_RESULTS_EXISTS;
        }
        conn.state = if conn.upsert.server_status & status::MORE_RESULTS_EXISTS != 0 {
            ConnState::NextResultPending
        } else {
            ConnState::Ready
        };
        if failed {
            conn.error.copy_from(&error);
            self.rows = Rows::Buffered { rows, current: 0 };
            return false;
        }
        conn.upsert.affected_rows = count;
        self.rows = Rows::Buffered { rows, current: 0 };
        true
    }

    /// Whether the rows are held whole.
    #[must_use]
    pub fn is_buffered(&self) -> bool {
        matches!(self.rows, Rows::Buffered { .. })
    }

    /// `mysqlnd_res::num_rows`: buffered rows' count; unbuffered rows'
    /// only once they are read to their end, `0` before, as libmysql.
    #[must_use]
    pub fn num_rows(&self) -> u64 {
        match &self.rows {
            Rows::Buffered { rows, .. } => rows.len() as u64,
            Rows::Unbuffered {
                eof: true,
                row_count,
            } => *row_count,
            _ => 0,
        }
    }

    /// Whether unbuffered rows were read to their end.
    #[must_use]
    pub fn eof(&self) -> bool {
        matches!(self.rows, Rows::Unbuffered { eof: true, .. })
    }

    /// `mysqlnd_res::data_seek`: buffered rows' cursor moved, past
    /// the end to the end; `false` for an unbuffered one.
    pub fn data_seek(&mut self, row: u64) -> bool {
        match &mut self.rows {
            Rows::Buffered { rows, current, .. } => {
                *current = usize::try_from(row).unwrap_or(usize::MAX).min(rows.len());
                true
            }
            _ => false,
        }
    }

    /// `mysqlnd_res::fetch_lengths`.
    #[must_use]
    pub fn lengths(&self) -> Option<&[u64]> {
        match &self.rows {
            Rows::Buffered { rows, current, .. } if *current > 0 && *current <= rows.len() => {
                Some(&self.lengths)
            }
            // `mysqlnd_result_unbuffered::fetch_lengths`, which a
            // statement's rows have none of: once a row was read or the
            // end was.
            Rows::Unbuffered { eof, .. } if !self.binary && (*eof || !self.lengths.is_empty()) => {
                Some(&self.lengths)
            }
            _ => None,
        }
    }

    /// `mysqlnd_res::fetch_row`: the next row's values, `Ok(None)` past
    /// the last, `Err(())` for php's `FAIL`. Unbuffered rows are read off
    /// `conn`.
    pub(crate) fn fetch_row(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        decode: bool,
    ) -> Result<Result<Option<Row>, ()>, Thrown> {
        let native = conn.options.int_and_float_native;
        if let (Some(id), Rows::Unbuffered { eof: false, .. }) = (self.cursor, &self.rows) {
            return self.fetch_cursor(req, conn, id, decode);
        }
        match &mut self.rows {
            Rows::None => Ok(Ok(None)),
            Rows::Buffered { rows, current, .. } => {
                if *current >= rows.len() {
                    if *current == rows.len() {
                        *current = rows.len() + 1;
                    }
                    return Ok(Ok(None));
                }
                let at = *current;
                *current += 1;
                conn.stats.global(
                    if self.binary {
                        Stat::RowsFetchedFromClientPsBuf
                    } else {
                        Stat::RowsFetchedFromClientNormalBuf
                    },
                    1,
                );
                if !decode {
                    return Ok(Ok(Some(Vec::new())));
                }
                let bytes = rows.get(at).cloned().unwrap_or_default();
                let decoded = self.decode(req, conn, &bytes, native)?;
                Ok(decoded.map(Some))
            }
            Rows::Unbuffered { eof, row_count } => {
                if *eof {
                    return Ok(Ok(None));
                }
                if conn.state != ConnState::FetchingData {
                    conn.error.client(cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC);
                    return Ok(Err(()));
                }
                let binary = self.binary;
                match read_row(req, conn, binary) {
                    RowPacket::Row(bytes) => {
                        *row_count += 1;
                        conn.stats.inc(if binary {
                            Stat::RowsFetchedFromClientPsUnbuf
                        } else {
                            Stat::RowsFetchedFromClientNormalUnbuf
                        });
                        if !decode {
                            return Ok(Ok(Some(Vec::new())));
                        }
                        let decoded = self.decode(req, conn, &bytes, native)?;
                        Ok(decoded.map(Some))
                    }
                    RowPacket::Eof {
                        warning_count,
                        server_status,
                    } => {
                        *eof = true;
                        conn.upsert.reset();
                        conn.upsert.warning_count = warning_count;
                        conn.upsert.server_status = server_status;
                        conn.state = if server_status & status::MORE_RESULTS_EXISTS != 0 {
                            ConnState::NextResultPending
                        } else {
                            ConnState::Ready
                        };
                        Ok(Ok(None))
                    }
                    RowPacket::Error(e) => {
                        e.set_on(&mut conn.error);
                        if conn.state != ConnState::QuitSent {
                            conn.state = ConnState::Ready;
                        }
                        *eof = true;
                        Ok(Err(()))
                    }
                    RowPacket::Fail => {
                        if conn.state != ConnState::QuitSent {
                            conn.state = ConnState::Ready;
                        }
                        *eof = true;
                        Ok(Err(()))
                    }
                }
            }
        }
    }

    /// The row decoder of the protocol, the lengths of a text row kept.
    fn decode(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        bytes: &[u8],
        native: bool,
    ) -> Result<Result<Row, ()>, Thrown> {
        let decoded = if self.binary {
            crate::codec::decode_binary(req, &mut conn.stats, bytes, &self.fields)?
        } else {
            decode_text(req, &mut conn.stats, bytes, &self.fields, native)?
        };
        if let Ok(row) = &decoded
            && !self.binary
        {
            self.lengths = row
                .iter()
                .map(|v| match v.as_ref() {
                    php_ext::values::ValueRef::Str(s) => s.len() as u64,
                    _ => 0,
                })
                .collect();
        }
        Ok(decoded)
    }

    /// `mysqlnd_res::skip_result`: unbuffered rows read to their end,
    /// counted as flushed.
    pub(crate) fn skip(&mut self, req: &mut Request, conn: &mut ConnData) -> Result<(), Thrown> {
        if !matches!(self.rows, Rows::Unbuffered { eof: false, .. }) {
            return Ok(());
        }
        conn.stats.inc(if self.kind == ResType::Normal {
            Stat::FlushedNormalSets
        } else {
            Stat::FlushedPsSets
        });
        while let Ok(Some(_)) = self.fetch_row(req, conn, false)? {
            conn.stats.inc(if self.kind == ResType::Normal {
                Stat::RowsSkippedNormal
            } else {
                Stat::RowsSkippedPs
            });
        }
        Ok(())
    }

    /// `mysqlnd_res::free_result`: counted by how it was freed, what is
    /// left of unbuffered rows read off the line.
    pub(crate) fn free(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        implicit: bool,
    ) -> Result<(), Thrown> {
        conn.stats.inc(if implicit {
            Stat::FreeResultImplicit
        } else {
            Stat::FreeResultExplicit
        });
        self.skip(req, conn)?;
        self.rows = Rows::None;
        Ok(())
    }

    /// `mysqlnd_res::fetch_field`: the next column's description.
    pub fn fetch_field(&mut self) -> Option<&Field> {
        let at = self.current_field;
        let field = self.fields.get(at)?;
        self.current_field += 1;
        Some(field)
    }

    /// `mysqlnd_res::field_seek`: the cursor moved, the old one answered.
    pub fn field_seek(&mut self, at: usize) -> usize {
        std::mem::replace(&mut self.current_field, at)
    }

    /// `mysqlnd_res::field_tell`.
    #[must_use]
    pub fn field_tell(&self) -> usize {
        self.current_field
    }
}

/// What one field packet came to.
enum FieldPacket {
    Field(Field),
    Error(PacketError),
    Fail,
}

/// `php_mysqlnd_rset_field_read`'s parse.
fn read_field(req: &mut Request, body: &[u8]) -> FieldPacket {
    let mut p = Body::new(body);
    if body.first() == Some(&ERROR_MARKER) {
        p.skip(1);
        return FieldPacket::Error(PacketError::read_from(p.rest()));
    }
    if body.first() == Some(&EODATA_MARKER) && body.len() < 8 {
        docref(
            req,
            Level::Warning,
            "Premature EOF in result field metadata",
        );
        return FieldPacket::Fail;
    }
    let mut parts: [Vec<u8>; 6] = Default::default();
    for part in &mut parts {
        let len = p.field_length();
        if len == NULL_LENGTH {
            docref(
                req,
                Level::Warning,
                "Protocol error. Server sent NULL_LENGTH. The server is faulty",
            );
            return FieldPacket::Fail;
        }
        *part = p.take(usize::try_from(len).unwrap_or(usize::MAX)).to_vec();
    }
    let [catalog, db, table, org_table, name, org_name] = parts;
    if body.get(p_at(&p)) != Some(&12) {
        docref(
            req,
            Level::Warning,
            "Protocol error. Server sent false length. Expected 12",
        );
    }
    if p_at(&p) + 12 > body.len() {
        docref(
            req,
            Level::Warning,
            "Premature end of data (mysqlnd_wireprotocol.c:1270)",
        );
        let short = p_at(&p) + 12 - body.len();
        docref(
            req,
            Level::Warning,
            &format!("Result set field packet {short} bytes shorter than expected"),
        );
        return FieldPacket::Fail;
    }
    p.skip(1);
    let charsetnr = u32::from(p.u16());
    let length = u64::from(p.u32());
    let type_byte = p.u8();
    let mut flags = u32::from(p.u16());
    let decimals = u32::from(p.u8());
    p.skip(2);
    let ty = FieldType::of(type_byte);
    let num = (ty.byte() <= FieldType::Int24.byte()
        && (ty != FieldType::Timestamp || length == 14 || length == 8))
        || ty == FieldType::Year;
    if num {
        flags |= column::NUM;
    }
    if p.left() > 0 {
        let len = p.field_length();
        if len != 0 && len != NULL_LENGTH {
            docref(
                req,
                Level::Warning,
                "Protocol error. Server sent default for unsupported field list (mysqlnd_wireprotocol.c:1302)",
            );
            return FieldPacket::Fail;
        }
    }
    let num_key = numeric_key(&name);
    FieldPacket::Field(Field {
        name,
        org_name,
        table,
        org_table,
        db,
        catalog,
        def: None,
        length,
        flags,
        decimals,
        charsetnr,
        type_byte,
        num_key,
    })
}

/// The body cursor's position.
fn p_at(p: &Body<'_>) -> usize {
    p.position()
}

/// `ZEND_HANDLE_NUMERIC_STR`: a name that is a decimal integer in
/// canonical form, no leading zero, `-0` and overflow excluded.
fn numeric_key(name: &[u8]) -> Option<i64> {
    let digits = name.strip_prefix(b"-").unwrap_or(name);
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits[0] == b'0')
        || name == b"-0"
    {
        return None;
    }
    std::str::from_utf8(name).ok()?.parse().ok()
}

/// What reading one row came to.
pub(crate) enum RowPacket {
    Row(Vec<u8>),
    Eof {
        warning_count: u32,
        server_status: u32,
    },
    Error(PacketError),
    /// The read failed: the connection is gone, its error set.
    Fail,
}

/// `php_mysqlnd_rowp_read` with `php_mysqlnd_read_row_ex`: a row's
/// bytes, a payload over several frames joined; an error or an EOF in
/// their place.
fn read_row(req: &mut Request, conn: &mut ConnData, binary: bool) -> RowPacket {
    let mut data = Vec::new();
    let mut header_size;
    loop {
        let Some(size) = conn.net.read_header(req, &mut conn.stats, &mut conn.error) else {
            conn.state = ConnState::QuitSent;
            conn.error
                .client(cr::SERVER_GONE_ERROR, crate::error::SERVER_GONE);
            return RowPacket::Fail;
        };
        header_size = size;
        let Some(chunk) = conn.net.receive(req, size, &mut conn.stats) else {
            conn.state = ConnState::QuitSent;
            conn.error
                .client(cr::SERVER_GONE_ERROR, crate::error::SERVER_GONE);
            return RowPacket::Fail;
        };
        data.extend_from_slice(&chunk);
        if size < crate::consts::MAX_PACKET_SIZE {
            break;
        }
    }
    conn.stats.add(
        Stat::BytesReceivedRsetRow,
        (crate::net::HEADER_SIZE + header_size) as u64,
    );
    conn.stats.inc(Stat::PacketsReceivedRsetRow);
    match data.first() {
        Some(&ERROR_MARKER) => {
            RowPacket::Error(PacketError::read_from(data.get(1..).unwrap_or_default()))
        }
        Some(&EODATA_MARKER) if data.len() < 8 => {
            let (mut warning_count, mut server_status) = (0, 0);
            if data.len() > 1 {
                let mut p = Body::new(&data[1..]);
                warning_count = u32::from(p.u16());
                server_status = u32::from(p.u16());
            }
            RowPacket::Eof {
                warning_count,
                server_status,
            }
        }
        _ => {
            conn.stats.inc(if binary {
                Stat::RowsFetchedFromServerPs
            } else {
                Stat::RowsFetchedFromServerNormal
            });
            RowPacket::Row(data)
        }
    }
}

/// `php_mysqlnd_rowp_read_text_protocol`: each column's length-encoded
/// text, the value [`text_value`] makes of it under
/// `MYSQLND_OPT_INT_AND_FLOAT_NATIVE` or without.
fn decode_text(
    req: &mut Request,
    stats: &mut crate::stats::Stats,
    bytes: &[u8],
    fields: &[Field],
    native: bool,
) -> Result<Result<Row, ()>, Thrown> {
    let mut p = Body::new(bytes);
    let mut row = Vec::with_capacity(fields.len());
    for field in fields {
        let len = p.field_length();
        if len == NULL_LENGTH {
            row.push(Owner::take(req, Value::null()));
            continue;
        }
        let Ok(len) = usize::try_from(len) else {
            return Ok(malformed(req, row));
        };
        if p.position() > bytes.len() || len > bytes.len() - p.position() {
            return Ok(malformed(req, row));
        }
        let text = p.take(len);
        let ty = field.field_type();
        stats.inc(text_stat(ty));
        stats.add(Stat::BytesReceivedPureDataText, len as u64);
        match text_value(req, field, text, native) {
            Ok(value) => row.push(value),
            Err(thrown) => {
                for value in row {
                    value.give_up(req);
                }
                return Err(thrown);
            }
        }
    }
    Ok(Ok(row))
}

/// One text column's value: a `BIT` column as its integer, written back
/// as a string where native types are off; with native types an integer
/// column as `atoll` (or `strtoull`, a string where it overflows) unless
/// it is zero-filled, a float column as `zend_strtod`; the text else.
fn text_value(
    req: &mut Request,
    field: &Field,
    text: &[u8],
    native: bool,
) -> Result<Owner<Value>, Thrown> {
    let ty = field.field_type();
    let (php_type, pack_len) = ty.fetch().unwrap_or((PhpType::String, 0));
    if ty == FieldType::Bit {
        return match crate::codec::bit_value(field, text) {
            crate::codec::Bits::Long(n) if !native => {
                PhpString::new(req, n.cast_unsigned().to_string().as_bytes()).map(Owner::into_value)
            }
            crate::codec::Bits::Long(n) => Ok(Owner::take(req, Value::int(n))),
            crate::codec::Bits::Text(s) => PhpString::new(req, s.as_bytes()).map(Owner::into_value),
        };
    }
    if native && php_type == PhpType::Long && field.flags & column::ZEROFILL == 0 {
        if pack_len < 8 {
            return Ok(Owner::take(req, Value::int(atoll(text))));
        }
        let v = strtoull(text);
        if field.flags & column::UNSIGNED != 0 && i64::try_from(v).is_err() {
            return PhpString::new(req, text).map(Owner::into_value);
        }
        return Ok(Owner::take(req, Value::int(v.cast_signed())));
    }
    if native && php_type == PhpType::Double {
        return Ok(Owner::take(req, Value::float(strtod(text))));
    }
    PhpString::new(req, text).map(Owner::into_value)
}

/// The warning of a field's length past the packet's end, the values
/// made so far given back.
fn malformed(req: &mut Request, row: Row) -> Result<Row, ()> {
    docref(
        req,
        Level::Warning,
        "Malformed server packet. Field length pointing after end of packet",
    );
    for value in row {
        value.give_up(req);
    }
    Err(())
}

/// `STAT_TEXT_TYPE_FETCHED_*` for a column's type.
fn text_stat(ty: FieldType) -> Stat {
    match ty {
        FieldType::Decimal | FieldType::NewDecimal => Stat::TextTypeFetchedDecimal,
        FieldType::Tiny => Stat::TextTypeFetchedInt8,
        FieldType::Short => Stat::TextTypeFetchedInt16,
        FieldType::Long => Stat::TextTypeFetchedInt32,
        FieldType::Float => Stat::TextTypeFetchedFloat,
        FieldType::Double => Stat::TextTypeFetchedDouble,
        FieldType::Null => Stat::TextTypeFetchedNull,
        FieldType::Timestamp => Stat::TextTypeFetchedTimestamp,
        FieldType::LongLong => Stat::TextTypeFetchedInt64,
        FieldType::Int24 => Stat::TextTypeFetchedInt24,
        FieldType::Date | FieldType::NewDate => Stat::TextTypeFetchedDate,
        FieldType::Time => Stat::TextTypeFetchedTime,
        FieldType::DateTime => Stat::TextTypeFetchedDatetime,
        FieldType::Year => Stat::TextTypeFetchedYear,
        FieldType::VarChar | FieldType::VarString | FieldType::String => {
            Stat::TextTypeFetchedString
        }
        FieldType::Bit => Stat::TextTypeFetchedBit,
        FieldType::Enum => Stat::TextTypeFetchedEnum,
        FieldType::Set => Stat::TextTypeFetchedSet,
        FieldType::Json => Stat::TextTypeFetchedJson,
        FieldType::TinyBlob
        | FieldType::MediumBlob
        | FieldType::LongBlob
        | FieldType::Blob
        | FieldType::Vector => Stat::TextTypeFetchedBlob,
        FieldType::Geometry => Stat::TextTypeFetchedGeometry,
        FieldType::Other(_) => Stat::TextTypeFetchedOther,
    }
}

/// C's `atoll`: optional space and sign, the digits, saturating.
fn atoll(text: &[u8]) -> i64 {
    let s = text.iter().skip_while(|b| b.is_ascii_whitespace()).copied();
    let mut negative = false;
    let mut out: i64 = 0;
    let mut first = true;
    for b in s {
        if first && (b == b'-' || b == b'+') {
            negative = b == b'-';
            first = false;
            continue;
        }
        first = false;
        if !b.is_ascii_digit() {
            break;
        }
        let d = i64::from(b - b'0');
        out = if negative {
            out.saturating_mul(10).saturating_sub(d)
        } else {
            out.saturating_mul(10).saturating_add(d)
        };
    }
    out
}

/// C's `strtoull(p, NULL, 10)`, saturating, a leading `-` negating as C
/// does.
fn strtoull(text: &[u8]) -> u64 {
    let s: Vec<u8> = text
        .iter()
        .copied()
        .skip_while(u8::is_ascii_whitespace)
        .collect();
    let (negative, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, &s[..]),
    };
    let mut out: u64 = 0;
    let mut over = false;
    for &b in digits.iter().take_while(|b| b.is_ascii_digit()) {
        match out
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(b - b'0')))
        {
            Some(v) => out = v,
            None => over = true,
        }
    }
    if over {
        return u64::MAX;
    }
    if negative { out.wrapping_neg() } else { out }
}

/// `zend_strtod(p, NULL)`: the longest prefix that is a decimal number.
fn strtod(text: &[u8]) -> f64 {
    let mut end = 0;
    let at = |i: usize| text.get(i).copied();
    if matches!(at(end), Some(b'-' | b'+')) {
        end += 1;
    }
    let digits_start = end;
    while at(end).is_some_and(|b| b.is_ascii_digit()) {
        end += 1;
    }
    if at(end) == Some(b'.') {
        end += 1;
        while at(end).is_some_and(|b| b.is_ascii_digit()) {
            end += 1;
        }
    }
    if end == digits_start || (end == digits_start + 1 && at(digits_start) == Some(b'.')) {
        return 0.0;
    }
    if matches!(at(end), Some(b'e' | b'E')) {
        let mut exp = end + 1;
        if matches!(at(exp), Some(b'-' | b'+')) {
            exp += 1;
        }
        if at(exp).is_some_and(|b| b.is_ascii_digit()) {
            while at(exp).is_some_and(|b| b.is_ascii_digit()) {
                exp += 1;
            }
            end = exp;
        }
    }
    std::str::from_utf8(&text[..end])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
}
