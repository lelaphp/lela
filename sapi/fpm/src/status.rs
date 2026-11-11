//! `fpm_status.c`: the ping page, the status page in its five forms —
//! plain, `html`, `xml`, `json` and `openmetrics`, each with the process
//! list under `full` — from a copy of the pool's `scoreboard`, and
//! `fpm_get_status()`'s array of the same.

use std::fmt::Write as _;

use php_sapi::{HostPage, HostValue};

use crate::conf::Pm;
use crate::scoreboard::{BoardView, ProcView, Stage};

const EXPIRES: &[u8] = b"Expires: Thu, 01 Jan 1970 00:00:00 GMT";
const CACHE_CONTROL: &[u8] = b"Cache-Control: no-cache, no-store, must-revalidate, max-age=0";

/// The ping page: `ping.response` as plain text.
pub(crate) fn ping(response: &[u8], head_only: bool) -> HostPage {
    HostPage {
        code: 200,
        headers: vec![
            b"Content-Type: text/plain".to_vec(),
            EXPIRES.to_vec(),
            CACHE_CONTROL.to_vec(),
        ],
        body: if head_only {
            Vec::new()
        } else {
            response.to_vec()
        },
    }
}

/// The status page's form, which key of `$_GET` names it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Plain,
    Html,
    Xml,
    Json,
    OpenMetrics,
}

/// `php_escape_html_entities_ex` under `ENT_DISALLOWED | ENT_HTML_DOC_XML1
/// | ENT_COMPAT` in UTF-8: `&`, `"`, `<` and `>` escaped, a character
/// XML 1.0 does not allow as `&#xFFFD;`, and nothing where the text is
/// not UTF-8.
fn escape_html(text: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(text) else {
        return Vec::new();
    };
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {
                out.push_str("&#xFFFD;");
            }
            c => out.push(c),
        }
    }
    out.into_bytes()
}

/// `php_json_encode_string` under `PHP_JSON_INVALID_UTF8_IGNORE`, without
/// its quotes: `"`, `\\` and `/` escaped, a control character and every
/// character past ASCII as `\uXXXX`, a byte that is not UTF-8 dropped.
fn escape_json(text: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(text).replace('\u{FFFD}', "");
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    write!(out, "\\u{unit:04x}").ok();
                }
            }
            c => out.push(c),
        }
    }
    out.into_bytes()
}

/// The time a status page writes, in the process's zone.
fn when(form: Form, epoch: i64) -> String {
    match form {
        Form::Html | Form::Plain => {
            String::from_utf8_lossy(&crate::ffi::strftime(b"%d/%b/%Y:%H:%M:%S %z", epoch, 63))
                .into_owned()
        }
        _ => epoch.to_string(),
    }
}

/// The page `fpm_status_handle_request` answers where it cannot read the
/// `scoreboard`.
fn internal_error() -> HostPage {
    HostPage {
        code: 500,
        headers: vec![
            b"Content-Type: text/plain".to_vec(),
            EXPIRES.to_vec(),
            CACHE_CONTROL.to_vec(),
        ],
        body: b"Internal error. Please review log file for errors.".to_vec(),
    }
}

/// `fpm_status_handle_request`'s status half over `board`, a copy taken
/// with its slots where `$_GET` names `full`, none where the copy failed;
/// `keys` are `$_GET`'s.
#[allow(clippy::too_many_lines)] // php's one function, kept as one
pub(crate) fn status(board: Option<&BoardView>, keys: &[Vec<u8>], head_only: bool) -> HostPage {
    let has = |key: &str| keys.iter().any(|k| k.as_slice() == key.as_bytes());
    let full = has("full");
    let mut headers = vec![EXPIRES.to_vec(), CACHE_CONTROL.to_vec()];
    let Some(board) = board else {
        crate::zlog!(
            Error @ fpm_status_handle_request:177,
            "status: unable to find or access status shared memory"
        );
        return internal_error();
    };
    if board.idle < 0 || board.active < 0 {
        crate::zlog!(
            Error @ fpm_status_handle_request:188,
            bytes "[pool {}] invalid status values",
            board.pool
        );
        return internal_error();
    }
    if head_only {
        return HostPage {
            code: 200,
            headers,
            body: Vec::new(),
        };
    }
    let (form, content_type): (Form, &[u8]) = if has("html") {
        (Form::Html, b"Content-Type: text/html")
    } else if has("xml") {
        (Form::Xml, b"Content-Type: text/xml")
    } else if has("json") {
        (Form::Json, b"Content-Type: application/json")
    } else if has("openmetrics") {
        (
            Form::OpenMetrics,
            b"Content-Type: application/openmetrics-text; version=1.0.0; charset=utf-8",
        )
    } else {
        (Form::Plain, b"Content-Type: text/plain")
    };
    headers.push(content_type.to_vec());
    let now = crate::scoreboard::epoch();
    let pool = String::from_utf8_lossy(&board.pool).into_owned();
    let pm = board.pm.map_or("", Pm::name);
    let since = u64::try_from(now - board.start_epoch).unwrap_or(0);
    let start = when(form, board.start_epoch);
    let total = board.idle + board.active;
    let b = board;
    let mut body = match form {
        Form::Html => format!(
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\" \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd\">\n\
<html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"en\" lang=\"en\">\n\
<head><title>PHP-FPM Status Page</title></head>\n\
<body>\n\
<table>\n\
<tr><th>pool</th><td>{pool}</td></tr>\n\
<tr><th>process manager</th><td>{pm}</td></tr>\n\
<tr><th>start time</th><td>{start}</td></tr>\n\
<tr><th>start since</th><td>{since}</td></tr>\n\
<tr><th>accepted conn</th><td>{}</td></tr>\n\
<tr><th>listen queue</th><td>{}</td></tr>\n\
<tr><th>max listen queue</th><td>{}</td></tr>\n\
<tr><th>listen queue len</th><td>{}</td></tr>\n\
<tr><th>idle processes</th><td>{}</td></tr>\n\
<tr><th>active processes</th><td>{}</td></tr>\n\
<tr><th>total processes</th><td>{total}</td></tr>\n\
<tr><th>max active processes</th><td>{}</td></tr>\n\
<tr><th>max children reached</th><td>{}</td></tr>\n\
<tr><th>slow requests</th><td>{}</td></tr>\n\
<tr><th>memory peak</th><td>{}</td></tr>\n\
</table>\n",
            b.requests, b.lq, b.lq_max, b.lq_len, b.idle, b.active, b.active_max, b.max_children_reached, b.slow_rq, b.memory_peak
        ),
        Form::Xml => format!(
            "<?xml version=\"1.0\" ?>\n<status>\n<pool>{pool}</pool>\n<process-manager>{pm}</process-manager>\n\
<start-time>{start}</start-time>\n<start-since>{since}</start-since>\n<accepted-conn>{}</accepted-conn>\n\
<listen-queue>{}</listen-queue>\n<max-listen-queue>{}</max-listen-queue>\n<listen-queue-len>{}</listen-queue-len>\n\
<idle-processes>{}</idle-processes>\n<active-processes>{}</active-processes>\n<total-processes>{total}</total-processes>\n\
<max-active-processes>{}</max-active-processes>\n<max-children-reached>{}</max-children-reached>\n\
<slow-requests>{}</slow-requests>\n<memory-peak>{}</memory-peak>\n",
            b.requests, b.lq, b.lq_max, b.lq_len, b.idle, b.active, b.active_max, b.max_children_reached, b.slow_rq, b.memory_peak
        ),
        Form::Json => format!(
            "{{\"pool\":\"{pool}\",\"process manager\":\"{pm}\",\"start time\":{start},\"start since\":{since},\
\"accepted conn\":{},\"listen queue\":{},\"max listen queue\":{},\"listen queue len\":{},\"idle processes\":{},\
\"active processes\":{},\"total processes\":{total},\"max active processes\":{},\"max children reached\":{},\
\"slow requests\":{},\"memory peak\":{}",
            b.requests, b.lq, b.lq_max, b.lq_len, b.idle, b.active, b.active_max, b.max_children_reached, b.slow_rq, b.memory_peak
        ),
        Form::OpenMetrics => format!(
            "# HELP phpfpm_up Could pool {pool} using a {pm} PM on PHP-FPM be reached?\n# TYPE phpfpm_up gauge\nphpfpm_up 1\n\
# HELP phpfpm_start_since The number of seconds since FPM has started.\n# TYPE phpfpm_start_since counter\nphpfpm_start_since {since}\n\
# HELP phpfpm_accepted_connections The number of requests accepted by the pool.\n# TYPE phpfpm_accepted_connections counter\nphpfpm_accepted_connections {}\n\
# HELP phpfpm_listen_queue The number of requests in the queue of pending connections.\n# TYPE phpfpm_listen_queue gauge\nphpfpm_listen_queue {}\n\
# HELP phpfpm_max_listen_queue The maximum number of requests in the queue of pending connections since FPM has started.\n# TYPE phpfpm_max_listen_queue counter\nphpfpm_max_listen_queue {}\n\
# TYPE phpfpm_listen_queue_length gauge\n# HELP phpfpm_listen_queue_length The size of the socket queue of pending connections.\nphpfpm_listen_queue_length {}\n\
# HELP phpfpm_idle_processes The number of idle processes.\n# TYPE phpfpm_idle_processes gauge\nphpfpm_idle_processes {}\n\
# HELP phpfpm_active_processes The number of active processes.\n# TYPE phpfpm_active_processes gauge\nphpfpm_active_processes {}\n\
# HELP phpfpm_total_processes The number of idle + active processes.\n# TYPE phpfpm_total_processes gauge\nphpfpm_total_processes {total}\n\
# HELP phpfpm_max_active_processes The maximum number of active processes since FPM has started.\n# TYPE phpfpm_max_active_processes counter\nphpfpm_max_active_processes {}\n\
# HELP phpfpm_max_children_reached The number of times, the process limit has been reached, when pm tries to start more children (works only for pm 'dynamic' and 'ondemand').\n# TYPE phpfpm_max_children_reached counter\nphpfpm_max_children_reached {}\n\
# HELP phpfpm_slow_requests The number of requests that exceeded your 'request_slowlog_timeout' value.\n# TYPE phpfpm_slow_requests counter\nphpfpm_slow_requests {}\n\
# HELP phpfpm_memory_peak The memory usage peak since FPM has started.\n# TYPE phpfpm_memory_peak gauge\nphpfpm_memory_peak {}\n# EOF\n",
            b.requests, b.lq, b.lq_max, b.lq_len, b.idle, b.active, b.active_max, b.max_children_reached, b.slow_rq, b.memory_peak
        ),
        Form::Plain => format!(
            "pool:                 {pool}\nprocess manager:      {pm}\nstart time:           {start}\nstart since:          {since}\n\
accepted conn:        {}\nlisten queue:         {}\nmax listen queue:     {}\nlisten queue len:     {}\n\
idle processes:       {}\nactive processes:     {}\ntotal processes:      {total}\nmax active processes: {}\n\
max children reached: {}\nslow requests:        {}\nmemory peak:          {}\n",
            b.requests, b.lq, b.lq_max, b.lq_len, b.idle, b.active, b.active_max, b.max_children_reached, b.slow_rq, b.memory_peak
        ),
    }
    .into_bytes();
    if !full {
        body.extend_from_slice(match form {
            Form::Html => b"</body></html>".as_slice(),
            Form::Xml => b"</status>",
            Form::Json => b"}",
            Form::OpenMetrics | Form::Plain => b"",
        });
        return HostPage {
            code: 200,
            headers,
            body,
        };
    }
    let (pre, separator, post): (&[u8], &[u8], &[u8]) = match form {
        Form::Html => (
            b"<table border=\"1\">\n<tr><th>pid</th><th>state</th><th>start time</th><th>start since</th><th>requests</th><th>request duration</th><th>request method</th><th>request uri</th><th>content length</th><th>user</th><th>script</th><th>last request cpu</th><th>last request memory</th></tr>\n",
            b"",
            b"</table></body></html>",
        ),
        Form::Xml => (b"<processes>\n", b"", b"</processes>\n</status>"),
        Form::Json => (b", \"processes\":[", b",", b"]}"),
        Form::OpenMetrics | Form::Plain => (b"", b"", b""),
    };
    if form == Form::OpenMetrics {
        return HostPage {
            code: 200,
            headers,
            body,
        };
    }
    body.extend_from_slice(pre);
    let now_us = crate::master::clock_us();
    let mut first = true;
    for proc in board.procs.iter().filter(|p| p.used) {
        if !first {
            body.extend_from_slice(separator);
        }
        first = false;
        body.extend_from_slice(&process(form, proc, now, now_us));
    }
    body.extend_from_slice(post);
    HostPage {
        code: 200,
        headers,
        body,
    }
}

/// One process of the full page, `full_syntax`.
fn process(form: Form, proc: &ProcView, now: i64, now_us: i64) -> Vec<u8> {
    let encode = |text: &[u8]| match form {
        Form::Html | Form::Xml => escape_html(text),
        Form::Json => escape_json(text),
        _ => text.to_vec(),
    };
    let uri = if proc.request_uri.is_empty() {
        b"-".to_vec()
    } else {
        encode(&proc.request_uri)
    };
    let query = (!proc.query_string.is_empty()).then(|| encode(&proc.query_string));
    let idle = proc.stage() == Stage::Accepting;
    let duration = if idle {
        proc.duration_us
    } else {
        now_us - proc.accepted_us
    };
    let cpu = if idle { proc.cpu() } else { 0.0 };
    let memory = if idle { proc.memory } else { 0 };
    let or_dash = |v: &[u8]| {
        if v.is_empty() {
            String::from("-")
        } else {
            String::from_utf8_lossy(v).into_owned()
        }
    };
    let method = or_dash(&proc.request_method);
    let user = or_dash(&proc.auth_user);
    let script = or_dash(&proc.script_filename);
    let request = format!(
        "{}{}{}",
        String::from_utf8_lossy(&uri),
        if query.is_some() { "?" } else { "" },
        String::from_utf8_lossy(query.as_deref().unwrap_or_default())
    );
    let start = when(form, proc.start_epoch);
    let since = u64::try_from(now - proc.start_epoch).unwrap_or(0);
    let (pid, state, requests, length) = (
        proc.pid,
        proc.stage().name(),
        proc.requests,
        proc.content_length,
    );
    let duration = u64::try_from(duration).unwrap_or(0);
    match form {
        Form::Html => format!("<tr><td>{pid}</td><td>{state}</td><td>{start}</td><td>{since}</td><td>{requests}</td><td>{duration}</td><td>{method}</td><td>{request}</td><td>{length}</td><td>{user}</td><td>{script}</td><td>{cpu:.2}</td><td>{memory}</td></tr>\n"),
        Form::Xml => format!("<process><pid>{pid}</pid><state>{state}</state><start-time>{start}</start-time><start-since>{since}</start-since><requests>{requests}</requests><request-duration>{duration}</request-duration><request-method>{method}</request-method><request-uri>{request}</request-uri><content-length>{length}</content-length><user>{user}</user><script>{script}</script><last-request-cpu>{cpu:.2}</last-request-cpu><last-request-memory>{memory}</last-request-memory></process>\n"),
        Form::Json => format!("{{\"pid\":{pid},\"state\":\"{state}\",\"start time\":{start},\"start since\":{since},\"requests\":{requests},\"request duration\":{duration},\"request method\":\"{method}\",\"request uri\":\"{request}\",\"content length\":{length},\"user\":\"{user}\",\"script\":\"{script}\",\"last request cpu\":{cpu:.2},\"last request memory\":{memory}}}"),
        _ => format!("\n************************\npid:                  {pid}\nstate:                {state}\nstart time:           {start}\nstart since:          {since}\nrequests:             {requests}\nrequest duration:     {duration}\nrequest method:       {method}\nrequest URI:          {request}\ncontent length:       {length}\nuser:                 {user}\nscript:               {script}\nlast request cpu:     {cpu:.2}\nlast request memory:  {memory}\n"),
    }
    .into_bytes()
}

/// `fpm_status_export_to_zval`: `fpm_get_status()`'s array.
pub(crate) fn export(board: &BoardView) -> HostValue {
    let now = crate::scoreboard::epoch();
    let now_us = crate::master::clock_us();
    let int = |n: i64| HostValue::Int(n);
    let text = |t: &[u8]| HostValue::Str(t.to_vec());
    let row = |k: &str, v: HostValue| (k.as_bytes().to_vec(), v);
    let procs = board
        .procs
        .iter()
        .filter(|p| p.used)
        .map(|proc| {
            let idle = proc.stage() == Stage::Accepting;
            let duration = if idle {
                proc.duration_us
            } else {
                now_us - proc.accepted_us
            };
            let or_dash = |v: &[u8]| if v.is_empty() { text(b"-") } else { text(v) };
            HostValue::Map(vec![
                row("pid", int(i64::from(proc.pid))),
                row("state", text(proc.stage().name().as_bytes())),
                row("start-time", int(proc.start_epoch)),
                row("start-since", int(now - proc.start_epoch)),
                row("requests", int(i64::try_from(proc.requests).unwrap_or(0))),
                row("request-duration", int(duration)),
                row("request-method", or_dash(&proc.request_method)),
                row("request-uri", text(&proc.request_uri)),
                row("query-string", text(&proc.query_string)),
                row(
                    "request-length",
                    int(i64::try_from(proc.content_length).unwrap_or(0)),
                ),
                row("user", or_dash(&proc.auth_user)),
                row("script", or_dash(&proc.script_filename)),
                row(
                    "last-request-cpu",
                    HostValue::Float(if idle { proc.cpu() } else { 0.0 }),
                ),
                row(
                    "last-request-memory",
                    int(if idle {
                        i64::try_from(proc.memory).unwrap_or(0)
                    } else {
                        0
                    }),
                ),
            ])
        })
        .collect();
    HostValue::Map(vec![
        row("pool", text(&board.pool)),
        row(
            "process-manager",
            text(board.pm.map_or("", Pm::name).as_bytes()),
        ),
        row("start-time", int(board.start_epoch)),
        row("start-since", int(now - board.start_epoch)),
        row(
            "accepted-conn",
            int(i64::try_from(board.requests).unwrap_or(0)),
        ),
        row("listen-queue", int(i64::from(board.lq))),
        row("max-listen-queue", int(i64::from(board.lq_max))),
        row("listen-queue-len", int(i64::from(board.lq_len))),
        row("idle-processes", int(i64::from(board.idle))),
        row("active-processes", int(i64::from(board.active))),
        row("total-processes", int(i64::from(board.idle + board.active))),
        row("max-active-processes", int(i64::from(board.active_max))),
        row(
            "max-children-reached",
            int(i64::from(board.max_children_reached)),
        ),
        row(
            "slow-requests",
            int(i64::try_from(board.slow_rq).unwrap_or(0)),
        ),
        row(
            "memory-peak",
            int(i64::try_from(board.memory_peak).unwrap_or(0)),
        ),
        row("procs", HostValue::List(procs)),
    ])
}
