//! `mysqlnd_result.c` and `mysqlnd_result_meta.c`: a set of rows, its
//! columns' descriptions, and its rows, held whole (`store_result`) or read
//! one at a time off the connection (`use_result`), each row kept as the
//! bytes the server sent and decoded into values when it is fetched, as
//! php's row buffers are. The set's structures, its column definitions
//! and its rows' packets are carved from its memory pool, on the request's
//! heap, at the sizes php's `get_chunk` asks for, and the stored rows'
//! `MYSQLND_ROW_BUFFER`s are one `emalloc` beside it, so
//! `memory_get_usage()` and `memory_limit` see a set of rows as php's do.

use php_ext::coerce::{is_space, strtol};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Owner, PhpString, Value};

use crate::alloc::PLUGIN_ROOM;
use crate::conn::ConnData;
use crate::consts::{ConnState, FieldType, MAX_PACKET_SIZE, PhpType, ResType, column, cr, status};
use crate::error::{ErrorInfo, OUT_OF_MEMORY, OUT_OF_SYNC};
use crate::pool::{Chunk, Pool};
use crate::stats::Stat;
use crate::wire::{Body, EODATA_MARKER, ERROR_MARKER, Length, Malformed, PacketError, Prot};

/// `sizeof(MYSQLND_RES)`.
const RES: usize = 272;
/// `sizeof(MYSQLND_RES_BUFFERED)`.
const RES_BUFFERED: usize = 736;
/// `sizeof(MYSQLND_RES_UNBUFFERED)`.
const RES_UNBUFFERED: usize = 104;
/// `sizeof(MYSQLND_RES_METADATA)`.
const RES_METADATA: usize = 24;
/// `sizeof(MYSQLND_FIELD)`.
const FIELD: usize = 152;
/// `sizeof(zval)`, of `row_data`'s.
const ZVAL: usize = 16;
/// `sizeof(size_t)`, of `lengths`'.
const LENGTH: usize = 8;
/// `sizeof(MYSQLND_ROW_BUFFER)`.
const ROW_BUFFER: usize = 16;
/// `sizeof(struct st_mysqlnd_packet_row)`, the packet `use_result`
/// allocates.
const PACKET_ROW: usize = 680;

/// `MYSQLND_FIELD`: one column as the server described it.
#[derive(Debug)]
pub struct Field {
    /// `sname`: one owner of the name `zend_string_init_interned` made,
    /// as opcache installs it ([`PhpString::existing_interned`]), the
    /// empty name `ZSTR_EMPTY_ALLOC()`; every copy of the column
    /// definitions takes one more ([`Field::copy`]) and each is given up with it
    /// ([`Field::free`]).
    pub name: Owner<PhpString>,
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
    /// `clone_metadata`'s copy of one field: the names' bytes copied and
    /// one more owner of `sname`, `zend_string_copy`
    /// (`mysqlnd_result_meta.c:157`).
    pub(crate) fn copy(&self, req: &Request) -> Field {
        Field {
            name: Owner::take(req, *self.name),
            org_name: self.org_name.clone(),
            table: self.table.clone(),
            org_table: self.org_table.clone(),
            db: self.db.clone(),
            catalog: self.catalog.clone(),
            def: self.def.clone(),
            length: self.length,
            flags: self.flags,
            decimals: self.decimals,
            charsetnr: self.charsetnr,
            type_byte: self.type_byte,
            num_key: self.num_key,
        }
    }

    /// `php_mysqlnd_free_field_metadata`: `sname` released,
    /// `zend_string_release_ex`.
    pub(crate) fn free(self, req: &mut Request) {
        self.name.give_up(req);
    }

    /// The column's type.
    #[must_use]
    pub fn field_type(&self) -> FieldType {
        FieldType::of(self.type_byte)
    }

    /// `root_len`: what `READ_RSET_FIELD` counts of the six names, each
    /// that is not empty with its terminator.
    fn root_len(&self) -> usize {
        [
            &self.catalog,
            &self.db,
            &self.table,
            &self.org_table,
            self.name.as_bytes(),
            &self.org_name,
        ]
        .iter()
        .filter(|part| !part.is_empty())
        .map(|part| part.len() + 1)
        .sum()
    }
}

/// `MYSQLND_RES_BUFFERED::row_buffers`: one `MYSQLND_ROW_BUFFER` per row
/// held, the place of its packet in the pool, in one `emalloc` grown as
/// `store_result_fetch_data` grows it, doubling to 1024 rows and by 1024
/// past them, and cut to the rows read at their end.
#[derive(Debug, Default)]
struct RowBuffers {
    block: Option<Emalloc>,
    /// `row_count`.
    count: usize,
    /// `free_rows`.
    room: usize,
}

impl RowBuffers {
    /// One row's chunk held; `Ok(false)` where its place does not fit the
    /// record, php's out of memory.
    fn push(&mut self, req: &mut Request, chunk: Chunk) -> Result<bool, Thrown> {
        let Some(record) = chunk.record() else {
            return Ok(false);
        };
        if self.room == 0 {
            let total = self.block.as_ref().map_or(0, |b| b.len() / ROW_BUFFER);
            let (room, total) = match total {
                0 => (1, 1),
                n if n < 1024 => (n, n * 2),
                n => (1024, n + 1024),
            };
            let size = total * ROW_BUFFER;
            self.block = Some(match self.block.take() {
                Some(block) => block.resize(req, size)?,
                None => Emalloc::new(req, size)?,
            });
            self.room = room;
        }
        self.room -= 1;
        if let Some(block) = &mut self.block {
            let at = self.count * ROW_BUFFER;
            block.bytes_mut()[at..at + ROW_BUFFER].copy_from_slice(&record);
        }
        self.count += 1;
        Ok(true)
    }

    /// "save some memory": the allocation cut to the rows held.
    fn shrink(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if self.room > 0
            && let Some(block) = self.block.take()
        {
            self.block = Some(block.resize(req, self.count * ROW_BUFFER)?);
            self.room = 0;
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.count
    }

    /// Row `at`'s chunk.
    fn get(&self, at: usize) -> Option<Chunk> {
        if at >= self.count {
            return None;
        }
        let bytes = self.block.as_ref()?.bytes();
        Chunk::from_record(bytes.get(at * ROW_BUFFER..(at + 1) * ROW_BUFFER)?)
    }

    /// `mnd_efree(set->row_buffers)`.
    fn free(self, req: &mut Request) {
        if let Some(block) = self.block {
            block.free(req);
        }
    }
}

/// How a `MYSQLND_RES`'s rows are held.
#[derive(Debug)]
enum Rows {
    /// Column definitions read, no rows asked for yet.
    None,
    /// `MYSQLND_RES_BUFFERED`.
    Buffered { rows: RowBuffers, current: usize },
    /// `MYSQLND_RES_UNBUFFERED`, with the `row_packet` `use_result`
    /// allocates; none for the rows that are only column definitions.
    Unbuffered {
        eof: bool,
        row_count: u64,
        packet: Option<Emalloc>,
    },
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
    /// `row_data`, the values a row is decoded into, its room made once
    /// for the set's columns and handed back empty by what took the
    /// values ([`Res::recycle`]), so a fetch allocates no row of its own.
    row_data: Row,
    /// The statement whose server-side cursor a statement's unbuffered
    /// rows are read from, row by row with `COM_STMT_FETCH`
    /// (`mysqlnd_fetch_stmt_row_cursor` in place of the unbuffered
    /// `fetch_row`).
    pub(crate) cursor: Option<u32>,
    /// The warnings and status the last cursor fetch read, which the
    /// statement takes into its own.
    pub(crate) cursor_status: (u32, u32),
    /// `memory_pool`.
    pool: Pool,
}

/// One row fetched: its values in column order, one owner of each.
pub type Row = Vec<Owner<Value>>;

impl Res {
    /// `mysqlnd_result_init`: the pool, the `MYSQLND_RES` and its `row_data`
    /// carved from it, the checkpoint a free goes back to saved; the
    /// `Err` is the memory limit's fatal.
    pub(crate) fn new(req: &mut Request, field_count: u32) -> Result<Res, Thrown> {
        let mut pool = Pool::create(req)?;
        pool.get_chunk(req, RES + PLUGIN_ROOM)?;
        pool.get_chunk(req, field_count as usize * ZVAL)?;
        pool.save();
        Ok(Res {
            field_count,
            fields: Vec::new(),
            current_field: 0,
            kind: ResType::Normal,
            rows: Rows::None,
            binary: false,
            lengths: Vec::new(),
            row_data: Vec::new(),
            cursor: None,
            cursor_status: (0, 0),
            pool,
        })
    }

    /// `mysqlnd_stmt_result_metadata`'s and `mysqlnd_stmt_get_result`'s
    /// `MYSQLND_RES`: a new one with a copy of `fields`, as
    /// `clone_metadata` carves it.
    pub(crate) fn with_fields(
        req: &mut Request,
        field_count: u32,
        fields: &[Field],
    ) -> Result<Res, Thrown> {
        let mut res = Res::new(req, field_count)?;
        res.clone_metadata(req, fields)?;
        Ok(res)
    }

    /// `result->unbuf` made with `eof_reached` set, before the column
    /// definitions: rows that are only those, as
    /// `mysqlnd_stmt_result_metadata` makes them.
    pub(crate) fn metadata_only(
        req: &mut Request,
        field_count: u32,
        fields: &[Field],
    ) -> Result<Res, Thrown> {
        let mut res = Res::new(req, field_count)?;
        res.unbuffered_init(req, true)?;
        res.rows = Rows::Unbuffered {
            eof: true,
            row_count: 0,
            packet: None,
        };
        res.binary = true;
        res.clone_metadata(req, fields)?;
        Ok(res)
    }

    /// `mysqlnd_res_meta::clone_metadata`: the column definitions'
    /// structures carved from the pool, each field's root beside them,
    /// and each field copied with one more owner of its name.
    fn clone_metadata(&mut self, req: &mut Request, fields: &[Field]) -> Result<(), Thrown> {
        self.pool.get_chunk(req, RES_METADATA)?;
        self.pool.get_chunk(req, (fields.len() + 1) * FIELD)?;
        for field in fields {
            self.pool.get_chunk(req, field.root_len())?;
        }
        self.fields = fields.iter().map(|field| field.copy(req)).collect();
        Ok(())
    }

    /// `mysqlnd_res_meta::free_metadata`: every field given up with its
    /// name's owner.
    fn free_fields(&mut self, req: &mut Request) {
        for field in self.fields.drain(..) {
            field.free(req);
        }
    }

    /// `mysqlnd_result_unbuffered_init`'s carving: the structure, and the
    /// lengths of text rows.
    fn unbuffered_init(&mut self, req: &mut Request, ps: bool) -> Result<(), Thrown> {
        self.pool.get_chunk(req, RES_UNBUFFERED + PLUGIN_ROOM)?;
        if !ps {
            self.pool
                .get_chunk(req, self.field_count as usize * LENGTH)?;
        }
        Ok(())
    }

    /// `mysqlnd_res::free_result_buffers`: the rows and the column
    /// definitions dropped, the pool back to the checkpoint `result_init`
    /// saved, the column count kept.
    pub(crate) fn free_buffers(&mut self, req: &mut Request) {
        match std::mem::replace(&mut self.rows, Rows::None) {
            Rows::Buffered { rows, .. } => rows.free(req),
            Rows::Unbuffered {
                packet: Some(packet),
                ..
            } => packet.free(req),
            _ => {}
        }
        self.free_fields(req);
        self.current_field = 0;
        self.cursor = None;
        self.pool.restore(req);
        self.pool.save();
    }

    /// `mysqlnd_res::free_result_contents_internal`: the rows dropped and
    /// the pool destroyed.
    pub(crate) fn release(mut self, req: &mut Request) {
        self.free_buffers(req);
        self.pool.destroy(req);
    }

    /// `mysqlnd_fetch_stmt_row_cursor`: one row asked of the statement's
    /// cursor and read, with the EOF that follows it, both out of the pool
    /// past a checkpoint the fetch goes back to; the warnings and status
    /// of the last packet kept on the connection and for the statement.
    fn fetch_cursor(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        stmt_id: u32,
        decode: bool,
    ) -> Result<Result<Option<Row>, ()>, Thrown> {
        conn.error.reset(req);
        let mut payload = stmt_id.to_le_bytes().to_vec();
        payload.extend_from_slice(&1u32.to_le_bytes());
        if !conn.send_command(req, crate::consts::Com::StmtFetch, &payload, false)? {
            return Ok(Err(()));
        }
        let saved = self.pool.enter();
        let fetched = self.cursor_row(req, conn, decode);
        self.pool.leave(req, saved);
        let (row, ok, last) = fetched?;
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

    /// The reads of [`Res::fetch_cursor`]: the row, whether every read
    /// passed, the last packet's warnings and status.
    #[allow(clippy::type_complexity)] // php's three out-parameters
    fn cursor_row(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        decode: bool,
    ) -> Result<(Option<Row>, bool, (u32, u32)), Thrown> {
        let native = conn.options.int_and_float_native;
        let (mut row, mut ok, mut last) = (None, true, (0, 0));
        match read_row(req, conn, &mut self.pool, true)? {
            RowPacket::Row(chunk) => {
                if decode {
                    let bytes = self.pool.bytes(chunk);
                    let into = std::mem::take(&mut self.row_data);
                    let decoded =
                        decode_row(req, conn, self.binary, &self.fields, bytes, native, into)?;
                    match decoded {
                        Ok(values) => row = Some(values),
                        Err(()) => return Ok((None, false, last)),
                    }
                }
                match read_row(req, conn, &mut self.pool, true)? {
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
        Ok((row, ok, last))
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
    /// the column definitions' structures carved from the pool, then one field
    /// packet per column, its names' root carved beside them.
    pub(crate) fn read_metadata(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
    ) -> Result<bool, Thrown> {
        self.free_fields(req);
        self.current_field = 0;
        self.pool.get_chunk(req, RES_METADATA + PLUGIN_ROOM)?;
        self.pool
            .get_chunk(req, (self.field_count as usize + 1) * FIELD)?;
        for _ in 0..self.field_count {
            let Some(body) = conn.read_packet(req, Prot::RsetField)? else {
                self.free_fields(req);
                return Ok(false);
            };
            match read_field(req, &body)? {
                FieldPacket::Field(field) => {
                    // The field is the metadata's before anything can
                    // fail, so a failure frees it with the rest.
                    let (root, type_byte) = (field.root_len(), field.type_byte);
                    self.fields.push(field);
                    self.pool.get_chunk(req, root)?;
                    if FieldType::of(type_byte).fetch().is_none() {
                        docref(
                            req,
                            Level::Warning,
                            &format!(
                                "Unknown type {type_byte} sent by the server. Please send a report to the developers"
                            ),
                        );
                        self.free_fields(req);
                        return Ok(false);
                    }
                }
                FieldPacket::Error(e) => {
                    self.free_fields(req);
                    e.set_on(req, &mut conn.error)?;
                    return Ok(false);
                }
                FieldPacket::Fail => {
                    self.free_fields(req);
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// `mysqlnd_res::use_result`: rows to be read one at a time, the
    /// unbuffered structure carved and its `row_packet` allocated; a
    /// statement's are the binary protocol's.
    pub(crate) fn use_unbuffered(&mut self, req: &mut Request, ps: bool) -> Result<(), Thrown> {
        self.kind = if ps {
            ResType::PsUnbuffered
        } else {
            ResType::Normal
        };
        self.binary = ps;
        self.unbuffered_init(req, ps)?;
        let packet = Emalloc::new(req, PACKET_ROW)?;
        self.rows = Rows::Unbuffered {
            eof: false,
            row_count: 0,
            packet: Some(packet),
        };
        self.lengths = Vec::new();
        Ok(())
    }

    /// `mysqlnd_res::store_result` with `store_result_fetch_data`: the
    /// buffered structure carved, every row read into the pool and held;
    /// the status of the last EOF taken; the affected rows the row count,
    /// as libmysql documents. The `Err` is the memory limit's fatal.
    ///
    /// What one set of rows holds past another is its packets, each in
    /// the block it was cut from, and its row buffers:
    ///
    /// ```php
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// $held = function (string $sql) use ($l) {
    ///     $m = memory_get_usage();
    ///     $r = mysqli_query($l, $sql);
    ///     $held = memory_get_usage() - $m;
    ///     mysqli_free_result($r);
    ///     return $held;
    /// };
    /// $one = $held("SELECT 'x' AS a");
    /// echo $held("SELECT REPEAT('x', 1000000) AS a, REPEAT('y', 1000000) AS b") - $one, "\n";
    /// echo $held("SELECT 'x' AS a FROM (WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 1000) SELECT n FROM s) t") - $one, "\n";
    /// ```
    /// ```expect mysql
    /// 4005888
    /// 16368
    /// ```
    ///
    /// Rows larger than `memory_limit` allows end the request in the
    /// limit's fatal, at the block the arena asks for:
    ///
    /// ```php
    /// ini_set('memory_limit', '4M');
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// $r = mysqli_query($l, "SELECT REPEAT('x', 3000000) AS a");
    /// echo "not reached\n";
    /// ```
    /// ```expect mysql exit=255
    /// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 3000032 bytes) in %s on line 5
    /// ```
    pub(crate) fn store(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        ps: bool,
    ) -> Result<bool, Thrown> {
        self.kind = if ps {
            ResType::PsBuffered
        } else {
            ResType::Normal
        };
        self.binary = ps;
        conn.state = ConnState::FetchingData;
        self.pool.get_chunk(req, RES_BUFFERED + PLUGIN_ROOM)?;
        if !ps {
            self.pool
                .get_chunk(req, self.field_count as usize * LENGTH)?;
        }
        let mut rows = RowBuffers::default();
        let read = self.store_rows(req, conn, ps, &mut rows);
        self.rows = Rows::Buffered { rows, current: 0 };
        let (failed, mut error) = read?;
        let count = match &self.rows {
            Rows::Buffered { rows, .. } => rows.len() as u64,
            _ => 0,
        };
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
        if let Rows::Buffered { rows, .. } = &mut self.rows {
            rows.shrink(req)?;
        }
        conn.state = if conn.upsert.server_status & status::MORE_RESULTS_EXISTS != 0 {
            ConnState::NextResultPending
        } else {
            ConnState::Ready
        };
        // `COPY_CLIENT_ERROR(conn->error_info, stored_data->error_info)`,
        // and the set's error info freed with the set.
        let copied = if failed {
            conn.error.copy_from(req, &error)
        } else {
            Ok(())
        };
        error.reset(req);
        copied?;
        if failed {
            return Ok(false);
        }
        conn.upsert.affected_rows = count;
        Ok(true)
    }

    /// The loop of `store_result_fetch_data`: rows read until the EOF,
    /// whose status is taken, or until a failure, whose error is answered.
    fn store_rows(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        ps: bool,
        rows: &mut RowBuffers,
    ) -> Result<(bool, ErrorInfo), Thrown> {
        let mut error = ErrorInfo::new(false);
        loop {
            match read_row(req, conn, &mut self.pool, ps)? {
                RowPacket::Row(chunk) => {
                    if !rows.push(req, chunk)? {
                        error.client(req, cr::OUT_OF_MEMORY, OUT_OF_MEMORY)?;
                        return Ok((true, error));
                    }
                }
                RowPacket::Eof {
                    warning_count,
                    server_status,
                } => {
                    conn.upsert.reset();
                    conn.upsert.warning_count = warning_count;
                    conn.upsert.server_status = server_status;
                    return Ok((false, error));
                }
                RowPacket::Error(e) => {
                    e.set_on(req, &mut error)?;
                    return Ok((true, error));
                }
                RowPacket::Fail => {
                    error.copy_from(req, &conn.error)?;
                    return Ok((true, error));
                }
            }
        }
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
                ..
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
    /// `conn` into the pool past a checkpoint the fetch goes back to.
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
        let at = match &mut self.rows {
            Rows::None => return Ok(Ok(None)),
            Rows::Buffered { rows, current, .. } => {
                if *current >= rows.len() {
                    if *current == rows.len() {
                        *current = rows.len() + 1;
                    }
                    return Ok(Ok(None));
                }
                let at = *current;
                *current += 1;
                rows.get(at)
            }
            Rows::Unbuffered { eof, .. } => {
                if *eof {
                    return Ok(Ok(None));
                }
                if conn.state != ConnState::FetchingData {
                    conn.error
                        .client(req, cr::COMMANDS_OUT_OF_SYNC, OUT_OF_SYNC)?;
                    return Ok(Err(()));
                }
                let saved = self.pool.enter();
                let fetched = self.fetch_unbuffered(req, conn, decode);
                self.pool.leave(req, saved);
                return fetched;
            }
        };
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
        let bytes = at.map_or(&[][..], |chunk| self.pool.bytes(chunk));
        let into = std::mem::take(&mut self.row_data);
        let decoded = decode_row(req, conn, self.binary, &self.fields, bytes, native, into)?;
        self.keep_lengths(&decoded);
        Ok(decoded.map(Some))
    }

    /// `mysqlnd_result_unbuffered::fetch_row`'s read: one row off the
    /// line into the pool and decoded, or the EOF or the error in its
    /// place.
    fn fetch_unbuffered(
        &mut self,
        req: &mut Request,
        conn: &mut ConnData,
        decode: bool,
    ) -> Result<Result<Option<Row>, ()>, Thrown> {
        let native = conn.options.int_and_float_native;
        let binary = self.binary;
        let packet = read_row(req, conn, &mut self.pool, binary)?;
        let Rows::Unbuffered { eof, row_count, .. } = &mut self.rows else {
            return Ok(Ok(None));
        };
        match packet {
            RowPacket::Row(chunk) => {
                *row_count += 1;
                conn.stats.inc(if binary {
                    Stat::RowsFetchedFromClientPsUnbuf
                } else {
                    Stat::RowsFetchedFromClientNormalUnbuf
                });
                if !decode {
                    return Ok(Ok(Some(Vec::new())));
                }
                let bytes = self.pool.bytes(chunk);
                let into = std::mem::take(&mut self.row_data);
                let decoded = decode_row(req, conn, binary, &self.fields, bytes, native, into)?;
                self.keep_lengths(&decoded);
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
                e.set_on(req, &mut conn.error)?;
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

    /// A fetched row handed back as the room the next row is decoded
    /// into, any value it still holds given up first, so the room is
    /// always empty when a decode takes it. The room is reused, never the values: each row fetched is its own,
    /// buffered, unbuffered or a statement's:
    ///
    /// ```php
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// $q = "SELECT 1 AS a, 'x' AS b UNION ALL SELECT 2, NULL UNION ALL SELECT 3, 'zz'";
    /// $r = mysqli_query($l, $q);
    /// echo json_encode([mysqli_fetch_row($r), mysqli_fetch_assoc($r), $r->fetch_object()]), "\n";
    /// $u = mysqli_query($l, $q, MYSQLI_USE_RESULT);
    /// echo json_encode([mysqli_fetch_row($u), mysqli_fetch_row($u), mysqli_fetch_row($u), mysqli_fetch_row($u)]), "\n";
    /// $s = mysqli_prepare($l, $q);
    /// $s->execute();
    /// $s->bind_result($a, $b);
    /// $seen = [];
    /// while ($s->fetch()) {
    ///     $seen[] = [$a, $b];
    /// }
    /// echo json_encode($seen), "\n";
    /// ```
    /// ```expect mysql
    /// [["1","x"],{"a":"2","b":null},{"a":"3","b":"zz"}]
    /// [["1","x"],["2",null],["3","zz"],null]
    /// [[1,"x"],[2,null],[3,"zz"]]
    /// ```
    pub(crate) fn recycle(&mut self, req: &mut Request, mut row: Row) {
        for value in row.drain(..) {
            value.give_up(req);
        }
        self.row_data = row;
    }

    /// The lengths of a text row decoded, kept for `fetch_lengths` in the
    /// one buffer of the set's columns, as php's `lengths` array is, each
    /// row's written over the last's:
    ///
    /// ```php
    /// mysqli_report(MYSQLI_REPORT_OFF);
    /// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
    /// $r = mysqli_query($l, "SELECT 'a', NULL UNION ALL SELECT 'bcd', 'xy'");
    /// mysqli_fetch_row($r);
    /// echo json_encode(mysqli_fetch_lengths($r)), "\n";
    /// mysqli_fetch_row($r);
    /// echo json_encode(mysqli_fetch_lengths($r)), "\n";
    /// var_dump(mysqli_fetch_row($r), mysqli_fetch_lengths($r));
    /// $u = mysqli_query($l, "SELECT 'abcd', 'x' UNION ALL SELECT '', NULL", MYSQLI_USE_RESULT);
    /// mysqli_fetch_row($u);
    /// echo json_encode(mysqli_fetch_lengths($u)), "\n";
    /// mysqli_fetch_row($u);
    /// echo json_encode(mysqli_fetch_lengths($u)), "\n";
    /// ```
    /// ```expect mysql
    /// [1,0]
    /// [3,2]
    /// NULL
    /// bool(false)
    /// [4,1]
    /// [0,0]
    /// ```
    fn keep_lengths(&mut self, decoded: &Result<Row, ()>) {
        if let Ok(row) = decoded
            && !self.binary
        {
            self.lengths.clear();
            self.lengths.extend(row.iter().map(|v| match v.as_ref() {
                php_ext::values::ValueRef::Str(s) => s.len() as u64,
                _ => 0,
            }));
        }
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
    /// left of unbuffered rows read off the line, the rows and the pool
    /// freed whatever the reading came to.
    pub(crate) fn free(
        mut self,
        req: &mut Request,
        conn: &mut ConnData,
        implicit: bool,
    ) -> Result<(), Thrown> {
        conn.stats.inc(if implicit {
            Stat::FreeResultImplicit
        } else {
            Stat::FreeResultExplicit
        });
        let skipped = self.skip(req, conn);
        self.release(req);
        skipped
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

/// The row decoder of the protocol, the values pushed onto `into`, an
/// empty row whose room is reused.
fn decode_row(
    req: &mut Request,
    conn: &mut ConnData,
    binary: bool,
    fields: &[Field],
    bytes: &[u8],
    native: bool,
    mut into: Row,
) -> Result<Result<Row, ()>, Thrown> {
    into.reserve(fields.len());
    if binary {
        crate::codec::decode_binary(req, &mut conn.stats, bytes, fields, into)
    } else {
        decode_text(req, &mut conn.stats, bytes, fields, native, into)
    }
}

/// What one field packet came to.
enum FieldPacket {
    Field(Field),
    Error(PacketError),
    Fail,
}

/// `php_mysqlnd_rset_field_read`'s parse, the column's name made as
/// opcache's `zend_string_init_interned` makes it
/// (`accel_init_interned_string_for_php`): the engine's string where its
/// table holds the name, else a string the column definitions own,
/// given back with the set of rows; the row's key holds one more owner of it:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $m = memory_get_usage();
/// foreach ([100, 1000, 10000, 100] as $n) {
///     $r = mysqli_query($l, "SELECT REPEAT('x', $n)");
///     mysqli_free_result($r);
///     echo memory_get_usage() - $m, "\n";
/// }
/// $r = mysqli_query($l, "SELECT 1 AS zzqqxxyyww");
/// echo memory_get_usage() - $m, "\n";
/// $row = mysqli_fetch_assoc($r);
/// mysqli_free_result($r);
/// echo memory_get_usage() - $m, "\n";
/// unset($row);
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect mysql regime=jit-off
/// 160
/// 192
/// 192
/// 192
/// 16664
/// 616
/// 192
/// ```
///
/// A name the engine's startup interned costs nothing, the lowercased
/// name a method or a class is keyed by, a constant's name, an ini
/// entry's name, an extension's lowercased name and a name of one byte,
/// where a name nobody declared is the set's, and the empty name is
/// `ZSTR_EMPTY_ALLOC()`:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_free_result(mysqli_query($l, "SELECT 1 AS a"));
/// foreach (["columnname", "arrayobject", "getiterator", "PHP_VERSION", "`date.timezone`", "pcre", "z, 3 AS ``", "c" . bin2hex(random_bytes(4))] as $name) {
///     $r = mysqli_query($l, "SELECT 2 AS $name");
///     $m = memory_get_usage();
///     mysqli_free_result($r);
///     echo $m - memory_get_usage(), "\n";
/// }
/// ```
/// ```expect mysql regime=jit-off
/// 16424
/// 16424
/// 16424
/// 16424
/// 16424
/// 16424
/// 16424
/// 16472
/// ```
///
/// A name a published unit interned costs nothing, as a string opcache
/// shares does: the strings a constant array holds as well as the
/// unit's own constant strings:
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// $names = ['zzqqxxyyww'];
/// $name = 'qqzzyyxxww';
/// $r = mysqli_query($l, "SELECT 2 AS zzqqxxyyww, 3 AS qqzzyyxxww");
/// $m = memory_get_usage();
/// mysqli_free_result($r);
/// echo $m - memory_get_usage(), "\n";
/// $r = mysqli_query($l, "SELECT 2 AS c" . bin2hex(random_bytes(4)) . ", 3 AS qqzzyyxxww");
/// $m = memory_get_usage();
/// mysqli_free_result($r);
/// echo $m - memory_get_usage(), "\n";
/// ```
/// ```expect mysql regime=jit-off
/// 16424
/// 16472
/// ```
///
/// A name no unit holds, such as `12345` from `SELECT 12345` or from an
/// emulated `SELECT ?` that is bound an integer, is a string the set owns. That
/// is opcache's answer and the model here, so a second set of the same name
/// makes it again. php -n differs, because the CLI has no opcache there:
/// `accel_startup` refuses it (`Z/ext/opcache/ZendAccelerator.c:3224`) and
/// never installs `accel_init_interned_string_for_php` (`:610`), which
/// answers with `zend_string_finit`'s size. Its `zend_string_init_interned`
/// stays `zend_string_init_interned_request` (`Z/Zend/zend_string.c:312`):
/// `zend_string_init`'s size, kept in `CG(interned_strings)` until the
/// request ends and found again at the next ask. So under php -n the name
/// costs 8 bytes less (`12345` takes the 32-byte bin, not finit's 40) and
/// only once. Whether the `trace:immutable-literal-arrays` row covers
/// that difference is a person's ruling, asked on #3425 (#3412):
///
/// ```php
/// mysqli_report(MYSQLI_REPORT_OFF);
/// $l = mysqli_connect('127.0.0.1', 'root', '', 'test');
/// mysqli_free_result(mysqli_query($l, "SELECT 1 AS a"));
/// foreach ([1, 2] as $i) {
///     $m = memory_get_usage();
///     $r = mysqli_query($l, "SELECT 12345");
///     echo memory_get_usage() - $m, " ";
///     mysqli_free_result($r);
///     echo memory_get_usage() - $m, "\n";
/// }
/// ```
/// ```expect mysql regime=jit-off
/// 16624 192
/// 16464 0
/// ```
///
/// A name's length past what is left of the packet is the faulty
/// server's, and the 12 bytes after the names are there whole:
///
/// ```php
/// set_error_handler(function ($no, $text) { echo preg_replace(['/PID=\d+/', '/:\d+$/'], ['PID', ''], $text), "\n"; return true; });
/// $serve = '$s = stream_socket_server("tcp://127.0.0.1:0"); echo stream_socket_get_name($s, false), "\n"; foreach (array_slice($argv, 1) as $talk) { $c = stream_socket_accept($s); foreach (explode(",", $talk) as $i => $hex) { if ($i) { fread($c, 65536); } fwrite($c, hex2bin($hex)); } while (in_array(substr((string) fread($c, 65536), 4, 1), ["\x01", "\x19"], true)) {} fclose($c); }';
/// $greet = '580000000a352e352e352d31302e352e31382d4d6172696144420003000000473e3f6047257c6700fef7080200ff81150000000000000f0000006c6b55463f49335f686c6431006d7973716c5f6e61746976655f70617373776f7264,0700000200000002000000';
/// $talks = ["$greet,0c00000100010000000100000000000000000002", "$greet,0c00000100010000000100000000000001000002ff", "$greet,0c0000010001000000010000000000003000000203646566087068705f74657374056974656d73056974656d73046974656dfe6974656d0ce000c8000000fd01100000001f000003fe00000200", "$greet,0c0000010001000000010000000000003000000203646566087068705f74657374056974656d73056974656d73046974656d0c6974656d0ce000c8000000fd011000000005000003fe00000200"];
/// $p = proc_open([PHP_BINARY, '-n', '-r', $serve, ...$talks], [1 => ['pipe', 'w']], $pipes);
/// $port = (int) explode(':', fgets($pipes[1]))[1];
/// mysqli_report(MYSQLI_REPORT_OFF);
/// foreach ($talks as $talk) { $m = new mysqli('127.0.0.1', 'root', 'secret', '', $port); var_dump($m->prepare('SELECT item FROM items')); $m->close(); }
/// proc_close($p);
/// ```
/// ```expect process
/// mysqli::prepare(): Premature end of data (mysqlnd_wireprotocol.c:1298)
/// mysqli::prepare(): Result set field packet shorter than expected
/// bool(false)
/// mysqli::prepare(): Premature end of data (mysqlnd_wireprotocol.c:1302)
/// mysqli::prepare(): Result set field packet shorter than expected
/// bool(false)
/// mysqli::prepare(): Protocol error. Server sent NULL_LENGTH. The server is faulty
/// bool(false)
/// mysqli::prepare(): Protocol error. Server sent false length. Expected 12
/// mysqli::prepare(): Premature end of data (mysqlnd_wireprotocol.c:1332)
/// mysqli::prepare(): Result set field packet shorter than expected
/// bool(false)
/// ```
fn read_field(req: &mut Request, body: &[u8]) -> Result<FieldPacket, Thrown> {
    let mut p = Body::new(body);
    let failed = |req: &mut Request, malformed: Malformed| {
        malformed.warn(req, Prot::RsetField);
        FieldPacket::Fail
    };
    if let Err(malformed) = p.need(1, 1298) {
        return Ok(failed(req, malformed));
    }
    if body.first() == Some(&ERROR_MARKER) {
        p.skip(1);
        if let Err(malformed) = p.need(1, 1302) {
            return Ok(failed(req, malformed));
        }
        return Ok(FieldPacket::Error(PacketError::read_from(p.rest())));
    }
    if body.first() == Some(&EODATA_MARKER) && body.len() < 8 {
        docref(
            req,
            Level::Warning,
            "Premature EOF in result field metadata",
        );
        return Ok(FieldPacket::Fail);
    }
    // `READ_RSET_FIELD`: a length past what is left, a `NULL` one among
    // them, is the faulty server's.
    let mut parts: [Vec<u8>; 6] = Default::default();
    for part in &mut parts {
        let len = p.field_length();
        let Some(bytes) = p.within(len) else {
            docref(
                req,
                Level::Warning,
                "Protocol error. Server sent NULL_LENGTH. The server is faulty",
            );
            return Ok(FieldPacket::Fail);
        };
        *part = bytes.to_vec();
    }
    let [catalog, db, table, org_table, name, org_name] = parts;
    if body.get(p.position()) != Some(&12) {
        docref(
            req,
            Level::Warning,
            "Protocol error. Server sent false length. Expected 12",
        );
    }
    if let Err(malformed) = p.need(12, 1332) {
        return Ok(failed(req, malformed));
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
    // `len < MYSQLND_INVALID_NET_FIELD_LENGTH`: any default at all, as
    // COM_FIELD_LIST is gone.
    if let Length::Of(_) = p.field_length() {
        docref(
            req,
            Level::Warning,
            "Protocol error. Server sent default for unsupported field list (mysqlnd_wireprotocol.c:1372)",
        );
        return Ok(FieldPacket::Fail);
    }
    let num_key = numeric_key(&name);
    // `meta->sname = zend_string_init_interned(meta->name, ...)`, the
    // empty name `ZSTR_EMPTY_ALLOC()`.
    let name = if name.is_empty() {
        PhpString::new(req, b"")?
    } else {
        PhpString::existing_interned(req, &name)?
    };
    Ok(FieldPacket::Field(Field {
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
    }))
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
    /// The row's packet, in the pool.
    Row(Chunk),
    Eof {
        warning_count: u32,
        server_status: u32,
    },
    Error(PacketError),
    /// The read failed: the connection is gone, its error set.
    Fail,
}

/// The connection gone under a read, as php marks it; the `Err` is the
/// memory limit's fatal.
fn gone(req: &mut Request, conn: &mut ConnData) -> Result<RowPacket, Thrown> {
    conn.state = ConnState::QuitSent;
    conn.error
        .client(req, cr::SERVER_GONE_ERROR, crate::error::SERVER_GONE)?;
    Ok(RowPacket::Fail)
}

/// `php_mysqlnd_rowp_read` with `php_mysqlnd_read_row_ex`: a row's
/// packet read into a chunk of `pool` one byte longer than it, frames of
/// `MYSQLND_MAX_PACKET_SIZE` joined in an `erealloc`'d buffer first and
/// copied in; an error or an EOF in its place. The `Err` is the memory
/// limit's fatal.
fn read_row(
    req: &mut Request,
    conn: &mut ConnData,
    pool: &mut Pool,
    binary: bool,
) -> Result<RowPacket, Thrown> {
    let Some(mut size) = conn.net.read_header(req, &mut conn.stats, &mut conn.error) else {
        return gone(req, conn);
    };
    let mut joined: Option<Emalloc> = None;
    let mut data_size = 0;
    while size >= MAX_PACKET_SIZE {
        let grown = match joined.take() {
            Some(buf) => buf.resize(req, data_size + size)?,
            None => Emalloc::new(req, data_size + size)?,
        };
        let buf = joined.insert(grown);
        let into = &mut buf.bytes_mut()[data_size..data_size + size];
        let read = conn.net.receive_into(req, into, &mut conn.stats);
        data_size += size;
        let next = if read {
            conn.net.read_header(req, &mut conn.stats, &mut conn.error)
        } else {
            None
        };
        let Some(next) = next else {
            if let Some(buf) = joined {
                buf.free(req);
            }
            return gone(req, conn);
        };
        size = next;
    }
    let chunk = pool.get_chunk(req, data_size + size + 1)?;
    if let Some(buf) = joined {
        pool.bytes_mut(chunk)[..data_size].copy_from_slice(&buf.bytes()[..data_size]);
        buf.free(req);
    }
    let into = &mut pool.bytes_mut(chunk)[data_size..data_size + size];
    if !conn.net.receive_into(req, into, &mut conn.stats) {
        return gone(req, conn);
    }
    let row = chunk.prefix(data_size + size);
    conn.stats.add(
        Stat::BytesReceivedRsetRow,
        (crate::net::HEADER_SIZE + size) as u64,
    );
    conn.stats.inc(Stat::PacketsReceivedRsetRow);
    let data = pool.bytes(row);
    Ok(match data.first() {
        Some(&ERROR_MARKER) => {
            RowPacket::Error(PacketError::read_from(data.get(1..).unwrap_or_default()))
        }
        Some(&EODATA_MARKER) if data.len() < 8 => {
            let (mut warning_count, mut server_status) = (0, 0);
            // The marker is followed by two bytes of warnings and two of
            // status, or by neither.
            if data.len() >= 5 {
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
            RowPacket::Row(row)
        }
    })
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
    mut row: Row,
) -> Result<Result<Row, ()>, Thrown> {
    let mut p = Body::new(bytes);
    for field in fields {
        let len = p.field_length();
        if len == Length::Null {
            row.push(Owner::null());
            continue;
        }
        let Some(text) = p.within(len) else {
            return Ok(malformed(req, row));
        };
        let len = text.len();
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
            bits => bits.into_value(req),
        };
    }
    if native && php_type == PhpType::Long && field.flags & column::ZEROFILL == 0 {
        if pack_len < 8 {
            // C's `atoll`, the long's `strtol` on a 64-bit host.
            return Ok(Owner::int(strtol(text)));
        }
        let v = strtoull(text);
        if field.flags & column::UNSIGNED != 0 && i64::try_from(v).is_err() {
            return PhpString::new(req, text).map(Owner::into_value);
        }
        return Ok(Owner::int(v.cast_signed()));
    }
    if native && php_type == PhpType::Double {
        return Ok(Owner::float(strtod(text)));
    }
    // `ZVAL_STRINGL_FAST` (`Z/ext/mysqlnd/mysqlnd_wireprotocol.c:1774`).
    PhpString::fast(req, text).map(Owner::into_value)
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

/// C's `strtoull(p, NULL, 10)`, saturating, a leading `-` negating as C
/// does: kept beside the face's signed `strtol`, the white space it skips
/// the face's `is_space`.
fn strtoull(text: &[u8]) -> u64 {
    let s: Vec<u8> = text.iter().copied().skip_while(|b| is_space(*b)).collect();
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
