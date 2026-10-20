//! `Z/ext/standard/mail.c`: `mail` over `php_mail`, which hands the
//! message to the `sendmail_path` command as php's `popen(3)` does: the
//! shell over the command and its extra parameters, in the request's
//! working directory, the message written to its standard input. The
//! additional headers are a string or php's array form built by
//! `php_mail_build_headers`, `mail.log` names a file or `syslog`,
//! `mail.add_x_header` prepends `X-PHP-Originating-Script`, and
//! `mail.cr_lf_mode` and `mail.mixed_lf_and_crlf` choose the line
//! separator, all as main.c declares them.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::io::Write;
use std::ops::ControlFlow;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;

use php_ext::abi::{Passed, Path, Union};
use php_ext::attr::php_function;
use php_ext::coerce::{c_str, is_space, value_name};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error};
use php_ext::request::{FormatDate, Ini, OwnedStream as _, Request, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

/// `EX_TEMPFAIL`, the one non-zero exit `php_mail` takes as sent.
const EX_TEMPFAIL: i32 = 75;

/// `LOG_NOTICE`, the priority `php_mail_log_to_syslog` logs at.
const LOG_NOTICE: i32 = 5;

/// `php_mail_build_headers_check_field_value` (`Z/ext/standard/mail.c:71`):
/// the error a header's value has, a folded `CRLF` or `LF` followed by a
/// space or tab being none.
fn check_field_value(value: &[u8]) -> Option<&'static str> {
    let at = |i: usize| value.get(i).copied().unwrap_or(0);
    let mut len = 0;
    while len < value.len() {
        match at(len) {
            b'\r' => {
                if at(len + 1) != b'\n' {
                    return Some("contains CR character that is not allowed in the header");
                }
                if value.len() - len >= 3 && matches!(at(len + 2), b' ' | b'\t') {
                    len += 3;
                    continue;
                }
                return Some(
                    "contains CRLF characters that are used as a line separator and are not allowed in the header",
                );
            }
            b'\n' => {
                if value.len() - len >= 2 && matches!(at(len + 1), b' ' | b'\t') {
                    len += 2;
                    continue;
                }
                return Some("contains LF character that is not allowed in the header");
            }
            0 => return Some("contains NULL character that is not allowed in the header"),
            _ => len += 1,
        }
    }
    None
}

/// `php_mail_build_headers_elem` (`Z/ext/standard/mail.c:133`): one
/// `Name: value` line, or php's error for the name or the value; an array
/// value is each of its elements under the name.
fn build_elem(
    req: &mut Request,
    out: &mut Vec<u8>,
    key: &[u8],
    value: &Value,
) -> Result<(), Thrown> {
    // Each message prints the name as `ZSTR_VAL(key)`, a C string.
    let name = String::from_utf8_lossy(c_str(key)).into_owned();
    match value.deref().as_ref() {
        ValueRef::Str(text) => {
            if key.iter().any(|&c| !(33..=126).contains(&c) || c == b':') {
                let message = format!("Header name \"{name}\" contains invalid characters");
                return Err(throw_error(req, "ValueError", &message));
            }
            if let Some(error) = check_field_value(text.as_bytes()) {
                let message = format!("Header \"{name}\" {error}");
                return Err(throw_error(req, "ValueError", &message));
            }
            out.extend_from_slice(key);
            out.extend_from_slice(b": ");
            let text = text.as_bytes();
            out.extend_from_slice(php_ext::coerce::c_str(text));
            out.extend_from_slice(b"\r\n");
            Ok(())
        }
        ValueRef::Array(array) => build_elems(req, out, key, *array),
        _ => {
            let given = value_name(req, &value.deref());
            let message = format!("Header \"{name}\" must be of type array|string, {given} given");
            Err(throw_error(req, "TypeError", &message))
        }
    }
}

/// `php_mail_build_headers_elems` (`Z/ext/standard/mail.c:177`): an
/// array header's elements, each a string under a numeric key.
fn build_elems(
    req: &mut Request,
    out: &mut Vec<u8>,
    key: &[u8],
    array: PhpArray,
) -> Result<(), Thrown> {
    let name = String::from_utf8_lossy(c_str(key)).into_owned();
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(val), …)`
    // (`Z/ext/standard/mail.c:182`), no owner of the table taken: the
    // argument holds it while a `TypeError` reaches the program.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let element_key = row.key().clone();
        let element = row.value().deref();
        if let Key::Str(found) = element_key {
            let found = String::from_utf8_lossy(c_str(found.as_bytes())).into_owned();
            let message =
                format!("Header \"{name}\" must only contain numeric keys, \"{found}\" found");
            return Err(throw_error(req, "TypeError", &message));
        }
        let element = element.deref();
        if !matches!(element.as_ref(), ValueRef::Str(_)) {
            let given = value_name(req, &element);
            let message =
                format!("Header \"{name}\" must only contain values of type string, {given} found");
            return Err(throw_error(req, "TypeError", &message));
        }
        build_elem(req, out, key, &element)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `php_mail_build_headers` (`Z/ext/standard/mail.c:211`): each header
/// by its name, `To` and `Subject` refused as additional ones, the ones
/// RFC 2822 allows once refusing an array, and the last `\r\n` taken
/// off.
pub fn build_headers(req: &mut Request, headers: PhpArray) -> Result<Vec<u8>, Thrown> {
    let mut out = Vec::new();
    // `ZEND_HASH_FOREACH_KEY_VAL(headers, …)`
    // (`Z/ext/standard/mail.c:218`), no owner of the table taken: the
    // argument holds it while a `TypeError` reaches the program.
    walk::foreach(req, headers, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        let key = match key {
            Key::Str(name) => name.as_bytes().to_vec(),
            Key::Int(idx) => {
                let message = format!("Header name cannot be numeric, {idx} given");
                return Err(throw_error(req, "TypeError", &message));
            }
        };
        let value = value.deref();
        let lower = key.to_ascii_lowercase();
        match lower.as_slice() {
            b"to" => {
                return Err(throw_error(
                    req,
                    "ValueError",
                    "The additional headers cannot contain the \"To\" header",
                ));
            }
            b"subject" => {
                return Err(throw_error(
                    req,
                    "ValueError",
                    "The additional headers cannot contain the \"Subject\" header",
                ));
            }
            b"orig-date" | b"from" | b"sender" | b"reply-to" | b"cc" | b"bcc" | b"message-id"
            | b"references" | b"in-reply-to"
                if value.as_array().is_some() =>
            {
                let target = String::from_utf8_lossy(&lower).into_owned();
                let message = format!("Header \"{target}\" must be of type string, array given");
                return Err(throw_error(req, "TypeError", &message));
            }
            _ => build_elem(req, &mut out, &key, &value)?,
        }
        Ok(ControlFlow::Continue(()))
    })?;
    out.truncate(out.len().saturating_sub(2));
    Ok(out)
}

/// The `to` and `subject` cleaning of `PHP_FUNCTION(mail)`
/// (`Z/ext/standard/mail.c:303-334`): trailing whitespace off, then each
/// control byte a space but a folded `\r\n` and the spaces after it,
/// read as the C string the value is.
fn clean(field: &[u8]) -> Vec<u8> {
    let field = php_ext::coerce::c_str(field);
    let end = field
        .iter()
        .rposition(|c| !is_space(*c))
        .map_or(0, |at| at + 1);
    let mut out = field[..end].to_vec();
    let at = |out: &[u8], i: usize| out.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while i < out.len() {
        if out[i].is_ascii_control() {
            if out[i] == b'\r'
                && at(&out, i + 1) == b'\n'
                && matches!(at(&out, i + 2), b' ' | b'\t')
            {
                i += 2;
                while matches!(at(&out, i + 1), b' ' | b'\t') {
                    i += 1;
                }
                i += 1;
                continue;
            }
            out[i] = b' ';
        }
        i += 1;
    }
    out
}

/// `php_mail_detect_multiple_crlf` (`Z/ext/standard/mail.c:406`).
fn multiple_crlf(hdr: &[u8]) -> bool {
    let at = |i: usize| hdr.get(i).copied().unwrap_or(0);
    let Some(&first) = hdr.first() else {
        return false;
    };
    if !(33..=126).contains(&first) || first == b':' {
        return true;
    }
    let mut i = 0;
    while at(i) != 0 {
        match at(i) {
            b'\r' => {
                if at(i + 1) == 0
                    || at(i + 1) == b'\r'
                    || (at(i + 1) == b'\n' && matches!(at(i + 2), 0 | b'\n' | b'\r'))
                {
                    return true;
                }
                i += 2;
            }
            b'\n' => {
                if matches!(at(i + 1), 0 | b'\r' | b'\n') {
                    return true;
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    false
}

/// `php_format_date("d-M-Y H:i:s e", now, 1)`: the stamp `mail.log`
/// lines begin with, in the request's default zone through the
/// [`FormatDate`] ext/date registers.
fn log_date(req: &mut Request) -> Result<Vec<u8>, Thrown> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let registered = req.registered::<FormatDate>().next().copied();
    match registered {
        Some(FormatDate(format)) => format(req, b"d-M-Y H:i:s e", now, true),
        None => Ok(Vec::new()),
    }
}

/// `php_mail` (`Z/ext/standard/mail.c:444`): the log line where
/// `mail.log` asks for one, the separator, the `X-PHP-Originating-Script`
/// header, php's refusal of malformed headers, then the command with the
/// message on its standard input, and its exit status read as php reads
/// it.
#[allow(clippy::too_many_lines)] // one C function, kept whole
pub fn php_mail(
    req: &mut Request,
    to: &[u8],
    subject: &[u8],
    message: &[u8],
    headers: Option<&[u8]>,
    extra_cmd: Option<&[u8]>,
) -> Result<bool, Thrown> {
    let ini = |req: &Request, name: &[u8]| req.ini().get(name).map(<[u8]>::to_vec);
    let mail_log = ini(req, b"mail.log").filter(|log| !log.is_empty());
    let file = req.current_file().unwrap_or(b"").to_vec();
    if let Some(log) = mail_log {
        let mut line = format!(
            "mail() on [{}:{}]: To: {} -- Headers: {} -- Subject: {}",
            String::from_utf8_lossy(&file),
            req.current_line(),
            String::from_utf8_lossy(to),
            String::from_utf8_lossy(headers.unwrap_or_default()),
            String::from_utf8_lossy(subject)
        )
        .into_bytes();
        if headers.is_some() {
            for c in &mut line {
                if matches!(*c, b'\r' | b'\n') {
                    *c = b' ';
                }
            }
        }
        if log == b"syslog" {
            crate::syslog::php_syslog(req, LOG_NOTICE, &line);
        } else {
            let mut entry = b"[".to_vec();
            entry.extend_from_slice(&log_date(req)?);
            entry.extend_from_slice(b"] ");
            entry.extend_from_slice(&line);
            entry.push(b'\n');
            // `REPORT_ERRORS | STREAM_DISABLE_OPEN_BASEDIR`: the log is
            // the system's, opened past the guard.
            let opening = php_ext::request::Opening {
                warn: true,
                disable_open_basedir: true,
                ..php_ext::request::Opening::default()
            };
            if let Some(stream) = req.open_stream_with(&log, b"a", opening)? {
                stream.write(req, &entry);
                stream.free(req);
            }
        }
    }
    // `if (EG(exception))`: a handler that threw on the log's warning
    // ends the call before anything is sent (GH-7875).
    php_ext::raising::pending(req)?;
    let mode = ini(req, b"mail.cr_lf_mode");
    let line_sep: &[u8] = match mode.as_deref() {
        Some(b"lf" | b"mixed" | b"os") => b"\n",
        _ if req
            .ini()
            .get(b"mail.mixed_lf_and_crlf")
            .is_some_and(Ini::parse_bool) =>
        {
            b"\n"
        }
        _ => b"\r\n",
    };
    let mut hdr = headers.map(<[u8]>::to_vec);
    if req
        .ini()
        .get(b"mail.add_x_header")
        .is_some_and(Ini::parse_bool)
    {
        let base = file
            .rsplit(|c| *c == b'/')
            .next()
            .unwrap_or_default()
            .to_vec();
        let mut x = format!(
            "X-PHP-Originating-Script: {}:",
            crate::pageinfo::getuid(req)
        )
        .into_bytes();
        x.extend_from_slice(&base);
        if let Some(headers) = hdr.filter(|h| !h.is_empty()) {
            x.extend_from_slice(line_sep);
            x.extend_from_slice(&headers);
        }
        hdr = Some(x);
    }
    if hdr.as_deref().is_some_and(multiple_crlf) {
        docref(
            req,
            Level::Warning,
            "Multiple or malformed newlines found in additional_header",
        );
        return Ok(false);
    }
    let sendmail_path = ini(req, b"sendmail_path").unwrap_or_default();
    let mut command = sendmail_path.clone();
    if let Some(extra) = extra_cmd {
        command.push(b' ');
        command.extend_from_slice(extra);
    }
    let child = crate::proc_open::shell(req, &command)
        .stdin(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        let text = format!(
            "Could not execute mail delivery program '{}'",
            String::from_utf8_lossy(&sendmail_path)
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    };
    let mut body = Vec::new();
    for (field, value) in [(b"To: ".as_slice(), to), (b"Subject: ", subject)] {
        body.extend_from_slice(field);
        body.extend_from_slice(value);
        body.extend_from_slice(line_sep);
    }
    if let Some(hdr) = &hdr {
        body.extend_from_slice(hdr);
        body.extend_from_slice(line_sep);
    }
    body.extend_from_slice(line_sep);
    let message = php_ext::coerce::c_str(message);
    if mode.as_deref() == Some(b"lf") {
        let mut i = 0;
        while i < message.len() {
            if message[i] == b'\r' && message.get(i + 1) == Some(&b'\n') {
                body.push(b'\n');
                i += 2;
            } else {
                body.push(message[i]);
                i += 1;
            }
        }
    } else {
        body.extend_from_slice(message);
    }
    body.extend_from_slice(line_sep);
    // The message written and sendmail waited for with the request lent
    // ([`Limits::blocking`]), as every wait on a child is (#2401); the
    // pipe closes before the wait, which is sendmail's end of input.
    let stdin = child.stdin.take();
    let waited = req.limits_mut().blocking(|| {
        if let Some(mut stdin) = stdin {
            stdin.write_all(&body).ok();
        }
        child.wait()
    });
    let status = match waited {
        Ok(status) => status,
        Err(error) => {
            let code = error.raw_os_error().unwrap_or(0);
            let text = format!(
                "Sendmail pclose failed {code} ({})",
                crate::filestat::strerror(&error)
            );
            docref(req, Level::Warning, &text);
            return Ok(false);
        }
    };
    if let Some(signal) = status.signal() {
        let text = format!(
            "Sendmail killed by signal {signal} ({})",
            crate::ffi::strsignal(signal)
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    let code = status.code().unwrap_or(0);
    if code != 0 && code != EX_TEMPFAIL {
        let text = format!("Sendmail exited with non-zero exit code {code}");
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    Ok(true)
}

/// `mail(string $to, string $subject, string $message, array|string $additional_headers = [], string $additional_params = ""): bool`,
/// `PHP_FUNCTION(mail)` (`Z/ext/standard/mail.c:273`): the three strings
/// as paths, the headers a string with no NUL, its trailing whitespace
/// off, or an array built by `php_mail_build_headers`; `to` and `subject`
/// cleaned of trailing whitespace and control bytes; the extra
/// parameters, or `mail.force_extra_parameters` over them, through
/// `escapeshellcmd`'s escape; then `php_mail`.
///
/// ```php
/// var_dump(mail('a@example.com', 's', 'm', "Bad\r\n\r\nX: y"), mail('a@example.com', 's', 'm', " leading"));
/// foreach ([['To' => 'x'], ['Subject' => 'x'], [1 => 'x'], ['From' => ['a']], ['X' => 1], ['X' => ['k' => 'v']], ['X' => [1]], ['Bad Name' => 'v'], ['X' => "a\rb"], ['X' => "a\nb"], ['X' => "a\r\nb"], ['X' => "a\0b"], ["\0foo" => 'v'], ["foo\0" => 'v']] as $headers) {
///     try { mail('a@example.com', 's', 'm', $headers); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// }
/// try { mail("a\0b", 's', 'm'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { mail('a', 's', 'm', "x\0y"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: mail(): Multiple or malformed newlines found in additional_header in %s on line 2
///
/// Warning: mail(): Multiple or malformed newlines found in additional_header in %s on line 2
/// bool(false)
/// bool(false)
/// ValueError: The additional headers cannot contain the "To" header
/// ValueError: The additional headers cannot contain the "Subject" header
/// TypeError: Header name cannot be numeric, 1 given
/// TypeError: Header "from" must be of type string, array given
/// TypeError: Header "X" must be of type array|string, int given
/// TypeError: Header "X" must only contain numeric keys, "k" found
/// TypeError: Header "X" must only contain values of type string, int found
/// ValueError: Header name "Bad Name" contains invalid characters
/// ValueError: Header "X" contains CR character that is not allowed in the header
/// ValueError: Header "X" contains LF character that is not allowed in the header
/// ValueError: Header "X" contains CRLF characters that are used as a line separator and are not allowed in the header
/// ValueError: Header "X" contains NULL character that is not allowed in the header
/// ValueError: Header name "" contains invalid characters
/// ValueError: Header name "foo" contains invalid characters
/// mail(): Argument #1 ($to) must not contain any null bytes
/// mail(): Argument #4 ($additional_headers) must not contain any null bytes
/// ```
///
/// ```php
/// echo new ReflectionFunction('mail');
/// ```
/// ```expect
/// Function [ <internal:standard> function mail ] {
///
///   - Parameters [5] {
///     Parameter #0 [ <required> string $to ]
///     Parameter #1 [ <required> string $subject ]
///     Parameter #2 [ <required> string $message ]
///     Parameter #3 [ <optional> array|string $additional_headers = [] ]
///     Parameter #4 [ <optional> string $additional_params = "" ]
///   }
///   - Return [ bool ]
/// }
/// ```
///
/// `mail.force_extra_parameters` read as `zend_ini_str_ex` reads it
/// ([`Ini::value`]): with no value the program's parameters are
/// handed on, and any value, the empty string among them, stands in
/// their place.
///
/// ```php
/// foreach ([[], ['-d', 'mail.force_extra_parameters='], ['-d', 'mail.force_extra_parameters=-fforced']] as $force) {
///     $p = proc_open([PHP_BINARY, '-n', '-d', 'sendmail_path="cat >/dev/null; echo sent:"', ...$force, '-r', 'var_dump(mail("a@example.com", "s", "m", "", "-fme"));'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
///     echo stream_get_contents($pipes[1]); proc_close($p);
/// }
/// ```
/// ```expect process
/// sent: -fme
/// bool(true)
/// sent:
/// bool(true)
/// sent: -fforced
/// bool(true)
/// ```
#[php_function]
fn mail(
    req: &mut Request,
    to: Path,
    subject: Path,
    message: Path,
    #[php(default = [])] additional_headers: Passed<Union<PhpArray, PhpString>>,
    #[php(default = "")] additional_params: Passed<Path>,
) -> Result<bool, Thrown> {
    let (Path(to), Path(subject), Path(message)) = (to, subject, message);
    let headers = match additional_headers {
        Passed::Given(Union::B(text)) => {
            if text.as_bytes().contains(&0) {
                return Err(arg_value_error(req, 4, "not contain any null bytes"));
            }
            let bytes = text.as_bytes();
            let end = bytes
                .iter()
                .rposition(|c| !matches!(c, b' ' | b'\n' | b'\r' | b'\t' | 0x0b | 0))
                .map_or(0, |at| at + 1);
            Some(bytes[..end].to_vec())
        }
        Passed::Given(Union::A(array)) => Some(build_headers(req, array)?),
        Passed::Omitted => None,
    };
    let extra = match additional_params {
        Passed::Given(Path(extra)) => Some(extra.as_bytes().to_vec()),
        Passed::Omitted => None,
    };
    // `zend_ini_str_ex`: a value, even the empty one, is forced over
    // the parameters; no value leaves them.
    let forced = req
        .ini()
        .value(b"mail.force_extra_parameters")
        .map(<[u8]>::to_vec);
    let extra = match forced.or(extra) {
        Some(extra) => Some(crate::exec::escape_shell_cmd(req, &extra)?),
        None => None,
    };
    let to = clean(to.as_bytes());
    let subject = clean(subject.as_bytes());
    let headers = headers.filter(|h| !h.is_empty());
    let sent = php_mail(
        req,
        &to,
        &subject,
        message.as_bytes(),
        headers.as_deref(),
        extra.as_deref(),
    )?;
    Ok(sent)
}
