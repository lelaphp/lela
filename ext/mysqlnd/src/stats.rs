//! `mysqlnd_statistics.c`: the `STAT_*` statistics, one per
//! `enum mysqlnd_collected_stats` variant, and their names as
//! `mysqlnd_stats_values_names` spells them. php reads the table by the
//! enum's index, and the two part where the text protocol has a JSON
//! statistic the table names only under the binary protocol, so every text
//! and binary statistic from there on reports the next row's name, as php's
//! does.
//!
//! **Where the counters live.** php counts each event twice under
//! `mysqlnd.collect_statistics`: on the connection and in
//! `mysqlnd_global_stats`, one table per process. Here the second table is
//! the request's (ADR-0005: no state an extension holds outlives the
//! request, and none is shared between requests), so what
//! `mysqli_get_client_stats` reads is php's under the CLI and starts at
//! zero for each request a server serves. A connection keeps what it
//! counted for the global table until the call it was lent to returns,
//! when [`Stats::flush`] adds it there.

/// `enum mysqlnd_collected_stats`, in php's order: every row of it, since
/// a statistic's place in the enum is its name's place in the table, so the
/// ones this crate does not count yet (the memory counters, which
/// `mysqlnd.collect_memory_statistics` turns on) are named too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
#[allow(dead_code)]
pub(crate) enum Stat {
    BytesSent,
    BytesReceived,
    PacketsSent,
    PacketsReceived,
    ProtocolOverheadIn,
    ProtocolOverheadOut,
    BytesReceivedOk,
    BytesReceivedEof,
    BytesReceivedRsetHeader,
    BytesReceivedRsetFieldMeta,
    BytesReceivedRsetRow,
    BytesReceivedPrepareResponse,
    BytesReceivedChangeUser,
    PacketsSentCmd,
    PacketsReceivedOk,
    PacketsReceivedEof,
    PacketsReceivedRsetHeader,
    PacketsReceivedRsetFieldMeta,
    PacketsReceivedRsetRow,
    PacketsReceivedPrepareResponse,
    PacketsReceivedChangeUser,
    RsetQuery,
    NonRsetQuery,
    NoIndexUsed,
    BadIndexUsed,
    QueryWasSlow,
    BufferedSets,
    UnbufferedSets,
    PsBufferedSets,
    PsUnbufferedSets,
    FlushedNormalSets,
    FlushedPsSets,
    PsPreparedNeverExecuted,
    PsPreparedOnceUsed,
    RowsFetchedFromServerNormal,
    RowsFetchedFromServerPs,
    RowsBufferedFromClientNormal,
    RowsBufferedFromClientPs,
    RowsFetchedFromClientNormalBuf,
    RowsFetchedFromClientNormalUnbuf,
    RowsFetchedFromClientPsBuf,
    RowsFetchedFromClientPsUnbuf,
    RowsFetchedFromClientPsCursor,
    RowsAffectedNormal,
    RowsAffectedPs,
    RowsSkippedNormal,
    RowsSkippedPs,
    CopyOnWriteSaved,
    CopyOnWritePerformed,
    CmdBufferTooSmall,
    ConnectSuccess,
    ConnectFailure,
    ConnectReused,
    Reconnect,
    PconnectSuccess,
    OpenedConnections,
    OpenedPersistentConnections,
    CloseExplicit,
    CloseImplicit,
    CloseDisconnect,
    CloseInMiddle,
    FreeResultExplicit,
    FreeResultImplicit,
    StmtCloseExplicit,
    StmtCloseImplicit,
    MemEmallocCount,
    MemEmallocAmount,
    MemEcallocCount,
    MemEcallocAmount,
    MemEreallocCount,
    MemEreallocAmount,
    MemEfreeCount,
    MemEfreeAmount,
    MemMallocCount,
    MemMallocAmount,
    MemCallocCount,
    MemCallocAmount,
    MemReallocCount,
    MemReallocAmount,
    MemFreeCount,
    MemFreeAmount,
    MemEstrndupCount,
    MemStrndupCount,
    MemEstrdupCount,
    MemStrdupCount,
    MemEdupCount,
    MemDupCount,
    TextTypeFetchedNull,
    TextTypeFetchedBit,
    TextTypeFetchedInt8,
    TextTypeFetchedInt16,
    TextTypeFetchedInt24,
    TextTypeFetchedInt32,
    TextTypeFetchedInt64,
    TextTypeFetchedDecimal,
    TextTypeFetchedFloat,
    TextTypeFetchedDouble,
    TextTypeFetchedDate,
    TextTypeFetchedYear,
    TextTypeFetchedTime,
    TextTypeFetchedDatetime,
    TextTypeFetchedTimestamp,
    TextTypeFetchedString,
    TextTypeFetchedJson,
    TextTypeFetchedBlob,
    TextTypeFetchedEnum,
    TextTypeFetchedSet,
    TextTypeFetchedGeometry,
    TextTypeFetchedOther,
    BinaryTypeFetchedNull,
    BinaryTypeFetchedBit,
    BinaryTypeFetchedInt8,
    BinaryTypeFetchedInt16,
    BinaryTypeFetchedInt24,
    BinaryTypeFetchedInt32,
    BinaryTypeFetchedInt64,
    BinaryTypeFetchedDecimal,
    BinaryTypeFetchedFloat,
    BinaryTypeFetchedDouble,
    BinaryTypeFetchedDate,
    BinaryTypeFetchedYear,
    BinaryTypeFetchedTime,
    BinaryTypeFetchedDatetime,
    BinaryTypeFetchedTimestamp,
    BinaryTypeFetchedString,
    BinaryTypeFetchedBlob,
    BinaryTypeFetchedEnum,
    BinaryTypeFetchedSet,
    BinaryTypeFetchedGeometry,
    BinaryTypeFetchedOther,
    InitCommandExecutedCount,
    InitCommandFailedCount,
    ComQuit,
    ComInitDb,
    ComQuery,
    ComFieldList,
    ComCreateDb,
    ComDropDb,
    ComRefresh,
    ComShutdown,
    ComStatistics,
    ComProcessInfo,
    ComConnect,
    ComProcessKill,
    ComDebug,
    ComPing,
    ComTime,
    ComDelayedInsert,
    ComChangeUser,
    ComBinlogDump,
    ComTableDump,
    ComConnectOut,
    ComRegisterSlave,
    ComStmtPrepare,
    ComStmtExecute,
    ComStmtSendLongData,
    ComStmtClose,
    ComStmtReset,
    ComSetOption,
    ComStmtFetch,
    ComDaemon,
    BytesReceivedPureDataText,
    BytesReceivedPureDataPs,
}

/// `STAT_LAST`: how many counters there are.
pub(crate) const COUNT: usize = 163;

/// `mysqlnd_stats_values_names`, indexed by [`Stat`].
pub(crate) const NAMES: [&str; COUNT] = [
    "bytes_sent",
    "bytes_received",
    "packets_sent",
    "packets_received",
    "protocol_overhead_in",
    "protocol_overhead_out",
    "bytes_received_ok_packet",
    "bytes_received_eof_packet",
    "bytes_received_rset_header_packet",
    "bytes_received_rset_field_meta_packet",
    "bytes_received_rset_row_packet",
    "bytes_received_prepare_response_packet",
    "bytes_received_change_user_packet",
    "packets_sent_command",
    "packets_received_ok",
    "packets_received_eof",
    "packets_received_rset_header",
    "packets_received_rset_field_meta",
    "packets_received_rset_row",
    "packets_received_prepare_response",
    "packets_received_change_user",
    "result_set_queries",
    "non_result_set_queries",
    "no_index_used",
    "bad_index_used",
    "slow_queries",
    "buffered_sets",
    "unbuffered_sets",
    "ps_buffered_sets",
    "ps_unbuffered_sets",
    "flushed_normal_sets",
    "flushed_ps_sets",
    "ps_prepared_never_executed",
    "ps_prepared_once_executed",
    "rows_fetched_from_server_normal",
    "rows_fetched_from_server_ps",
    "rows_buffered_from_client_normal",
    "rows_buffered_from_client_ps",
    "rows_fetched_from_client_normal_buffered",
    "rows_fetched_from_client_normal_unbuffered",
    "rows_fetched_from_client_ps_buffered",
    "rows_fetched_from_client_ps_unbuffered",
    "rows_fetched_from_client_ps_cursor",
    "rows_affected_normal",
    "rows_affected_ps",
    "rows_skipped_normal",
    "rows_skipped_ps",
    "copy_on_write_saved",
    "copy_on_write_performed",
    "command_buffer_too_small",
    "connect_success",
    "connect_failure",
    "connection_reused",
    "reconnect",
    "pconnect_success",
    "active_connections",
    "active_persistent_connections",
    "explicit_close",
    "implicit_close",
    "disconnect_close",
    "in_middle_of_command_close",
    "explicit_free_result",
    "implicit_free_result",
    "explicit_stmt_close",
    "implicit_stmt_close",
    "mem_emalloc_count",
    "mem_emalloc_amount",
    "mem_ecalloc_count",
    "mem_ecalloc_amount",
    "mem_erealloc_count",
    "mem_erealloc_amount",
    "mem_efree_count",
    "mem_efree_amount",
    "mem_malloc_count",
    "mem_malloc_amount",
    "mem_calloc_count",
    "mem_calloc_amount",
    "mem_realloc_count",
    "mem_realloc_amount",
    "mem_free_count",
    "mem_free_amount",
    "mem_estrndup_count",
    "mem_strndup_count",
    "mem_estrdup_count",
    "mem_strdup_count",
    "mem_edupl_count",
    "mem_dupl_count",
    "proto_text_fetched_null",
    "proto_text_fetched_bit",
    "proto_text_fetched_tinyint",
    "proto_text_fetched_short",
    "proto_text_fetched_int24",
    "proto_text_fetched_int",
    "proto_text_fetched_bigint",
    "proto_text_fetched_decimal",
    "proto_text_fetched_float",
    "proto_text_fetched_double",
    "proto_text_fetched_date",
    "proto_text_fetched_year",
    "proto_text_fetched_time",
    "proto_text_fetched_datetime",
    "proto_text_fetched_timestamp",
    "proto_text_fetched_string",
    "proto_text_fetched_blob",
    "proto_text_fetched_enum",
    "proto_text_fetched_set",
    "proto_text_fetched_geometry",
    "proto_text_fetched_other",
    "proto_binary_fetched_null",
    "proto_binary_fetched_bit",
    "proto_binary_fetched_tinyint",
    "proto_binary_fetched_short",
    "proto_binary_fetched_int24",
    "proto_binary_fetched_int",
    "proto_binary_fetched_bigint",
    "proto_binary_fetched_decimal",
    "proto_binary_fetched_float",
    "proto_binary_fetched_double",
    "proto_binary_fetched_date",
    "proto_binary_fetched_year",
    "proto_binary_fetched_time",
    "proto_binary_fetched_datetime",
    "proto_binary_fetched_timestamp",
    "proto_binary_fetched_string",
    "proto_binary_fetched_json",
    "proto_binary_fetched_blob",
    "proto_binary_fetched_enum",
    "proto_binary_fetched_set",
    "proto_binary_fetched_geometry",
    "proto_binary_fetched_other",
    "init_command_executed_count",
    "init_command_failed_count",
    "com_quit",
    "com_init_db",
    "com_query",
    "com_field_list",
    "com_create_db",
    "com_drop_db",
    "com_refresh",
    "com_shutdown",
    "com_statistics",
    "com_process_info",
    "com_connect",
    "com_process_kill",
    "com_debug",
    "com_ping",
    "com_time",
    "com_delayed_insert",
    "com_change_user",
    "com_binlog_dump",
    "com_table_dump",
    "com_connect_out",
    "com_register_slave",
    "com_stmt_prepare",
    "com_stmt_execute",
    "com_stmt_send_long_data",
    "com_stmt_close",
    "com_stmt_reset",
    "com_stmt_set_option",
    "com_stmt_fetch",
    "com_deamon",
    "bytes_received_real_data_normal",
    "bytes_received_real_data_ps",
];

/// `MYSQLND_STATS`: a connection's counters, and what it counted since
/// the last [`Stats::flush`] for the request's table.
#[derive(Debug, Clone, php_ext::abi::EngineState)]
pub(crate) struct Stats {
    /// Whether `mysqlnd.collect_statistics` is on for the call in hand.
    pub(crate) enabled: bool,
    pub(crate) values: Box<[u64; COUNT]>,
    unflushed: Box<[u64; COUNT]>,
}

impl Default for Stats {
    fn default() -> Stats {
        Stats {
            enabled: true,
            values: Box::new([0; COUNT]),
            unflushed: Box::new([0; COUNT]),
        }
    }
}

impl Stats {
    /// `MYSQLND_INC_CONN_STATISTIC_W_VALUE`: the connection's statistic and
    /// the global one moved by `value`, wrapping as php's `uint64_t` does
    /// for the `-1` a close adds.
    pub(crate) fn add(&mut self, stat: Stat, value: u64) {
        if !self.enabled {
            return;
        }
        let at = stat as usize;
        if let (Some(conn), Some(global)) = (self.values.get_mut(at), self.unflushed.get_mut(at)) {
            *conn = conn.wrapping_add(value);
            *global = global.wrapping_add(value);
        }
    }

    /// `MYSQLND_INC_CONN_STATISTIC`.
    pub(crate) fn inc(&mut self, stat: Stat) {
        self.add(stat, 1);
    }

    /// `MYSQLND_INC_GLOBAL_STATISTIC`: the global statistic alone.
    pub(crate) fn global(&mut self, stat: Stat, value: u64) {
        if !self.enabled {
            return;
        }
        if let Some(global) = self.unflushed.get_mut(stat as usize) {
            *global = global.wrapping_add(value);
        }
    }

    /// What was counted for the global table since the last flush, added
    /// to `global` and forgotten here.
    pub(crate) fn flush(&mut self, global: &mut [u64]) {
        for (to, from) in global.iter_mut().zip(self.unflushed.iter_mut()) {
            *to = to.wrapping_add(*from);
            *from = 0;
        }
    }
}
