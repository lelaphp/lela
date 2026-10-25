//! `session.upload_progress`, `php_session_rfc1867_callback`
//! (`Z/ext/session/session.c:3169`): while a multipart body is read, the
//! file uploads a form names under `session.upload_progress.name` are
//! written into the session its cookie, query string or form names, as
//! an array under `session.upload_progress.prefix` and that name, at
//! every step of `session.upload_progress.freq` bytes and no oftener
//! than `min_freq` seconds, and taken out again at the end under
//! `session.upload_progress.cleanup`. It is the engine's upload hook
//! ([`php_ext::abi::ExtensionBuilder::upload`]), served while the request
//! starts, before the session's own `RINIT`.

use php_ext::abi::{Beneath, chain};
use php_ext::coerce::atoi;
use php_ext::raising::Thrown;
use php_ext::request::{Input, Request, Upload};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, walk};

use crate::{
    Phase, State, Status, absorb, ini, ini_bool, session, session_array_mut, string_value,
};

/// `php_session_rfc1867_progress`: what one body's reading keeps.
#[derive(Debug, Default)]
pub(crate) struct Progress {
    content_length: i64,
    /// The id the form, the cookie or the query string named.
    sid: Option<Vec<u8>>,
    /// The key the progress is written under.
    key: Option<Vec<u8>>,
    apply_trans_sid: bool,
    update_step: i64,
    next_update: i64,
    next_update_time: f64,
    /// `progress->data`, from the first file on.
    data: Option<Data>,
    cancel_upload: bool,
}

/// The progress array's fields, in php's order.
#[derive(Debug, Default)]
struct Data {
    start_time: i64,
    content_length: i64,
    bytes_processed: i64,
    done: bool,
    files: Vec<File>,
}

/// One file's array, in php's order.
#[derive(Debug, Default)]
struct File {
    field_name: Vec<u8>,
    name: Vec<u8>,
    tmp_name: Option<Vec<u8>>,
    error: i64,
    done: bool,
    start_time: i64,
    bytes_processed: i64,
}

/// The upload hook: `php_session_rfc1867_callback` over one step, which
/// serves the link it saved first and keeps its answer
/// (`Z/ext/session/session.c:3174-3176`), and `false`, php's `FAILURE`,
/// where that link failed or once the session asked for the upload to be
/// cancelled.
///
/// ```php
/// var_dump(ini_get('session.upload_progress.enabled'), ini_get('session.upload_progress.name'));
/// ```
/// ```expect
/// string(1) "1"
/// string(27) "PHP_SESSION_UPLOAD_PROGRESS"
/// ```
pub(crate) fn hook(req: &mut Request, beneath: Beneath<'_, chain::Upload>) -> Result<bool, Thrown> {
    let went_on = beneath.serve(req)?;
    Ok(step(req, &beneath.event())? && went_on)
}

/// The session's own work over one step of the upload; `false` once the
/// session asked for the upload to be cancelled.
fn step(req: &mut Request, event: &Upload<'_>) -> Result<bool, Thrown> {
    if !ini_bool(req, b"session.upload_progress.enabled") {
        return Ok(true);
    }
    // "request_parse_body(). Do not run here as it corrupts the session."
    if req.state::<State>().phase == Phase::Activated {
        return Ok(true);
    }
    match *event {
        Upload::Start { content_length } => {
            // "This runs before RINIT, so we need to clear these early."
            session::rinit_globals(req);
            req.state::<State>().progress = Some(Progress {
                content_length,
                ..Progress::default()
            });
        }
        Upload::FormData { name, value, .. } => form_data(req, name, value)?,
        Upload::FileStart {
            read,
            name,
            filename,
        } => file_start(req, read, name, filename)?,
        Upload::FileData {
            read,
            offset,
            length,
        } => {
            if tracking(req) {
                with(req, |progress| {
                    if let Some(data) = progress.data.as_mut() {
                        if let Some(file) = data.files.last_mut() {
                            file.bytes_processed = offset + length;
                        }
                        data.bytes_processed = read;
                    }
                });
                update(req, false)?;
            }
        }
        Upload::FileEnd { read, temp, error } => {
            if tracking(req) {
                with(req, |progress| {
                    if let Some(data) = progress.data.as_mut() {
                        if let Some(file) = data.files.last_mut() {
                            if let Some(temp) = temp {
                                file.tmp_name = Some(temp.to_vec());
                            }
                            file.error = error;
                            file.done = true;
                        }
                        data.bytes_processed = read;
                    }
                });
                update(req, false)?;
            }
        }
        Upload::End { read } => {
            end(req, read)?;
            return Ok(true);
        }
    }
    Ok(!req
        .state::<State>()
        .progress
        .as_ref()
        .is_some_and(|progress| progress.cancel_upload))
}

/// Whether the body names both a session and a progress key.
fn tracking(req: &mut Request) -> bool {
    req.state::<State>()
        .progress
        .as_ref()
        .is_some_and(|progress| progress.sid.is_some() && progress.key.is_some())
}

/// The progress, changed by `change`, where there is one.
fn with(req: &mut Request, change: impl FnOnce(&mut Progress)) {
    if let Some(progress) = req.state::<State>().progress.as_mut() {
        change(progress);
    }
}

/// `MULTIPART_EVENT_FORMDATA`: the form's session id, or its progress key,
/// which looks for the id in the cookie and the query string at once.
fn form_data(req: &mut Request, name: &[u8], value: &[u8]) -> Result<(), Thrown> {
    if tracking(req) || value.is_empty() {
        return Ok(());
    }
    let session_name = ini(req, b"session.name");
    if name == session_name.as_slice() {
        with(req, |progress| progress.sid = Some(value.to_vec()));
    } else if name == ini(req, b"session.upload_progress.name").as_slice() {
        let mut key = ini(req, b"session.upload_progress.prefix");
        key.extend_from_slice(value);
        with(req, |progress| {
            progress.key = Some(key);
            progress.apply_trans_sid = true;
        });
        early_find_sid(req, &session_name)?;
    }
    Ok(())
}

/// `php_session_rfc1867_early_find_sid`: the id the cookie carries, and
/// where cookies are not all, the query string's.
fn early_find_sid(req: &mut Request, session_name: &[u8]) -> Result<(), Thrown> {
    if ini_bool(req, b"session.use_cookies") {
        let found = find_in(req, Input::Cookie, session_name)?;
        if let Some(found) = found {
            with(req, |progress| {
                progress.sid = Some(found);
                progress.apply_trans_sid = false;
            });
            return Ok(());
        }
    }
    if ini_bool(req, b"session.use_only_cookies") {
        return Ok(());
    }
    if let Some(found) = find_in(req, Input::Get, session_name)? {
        with(req, |progress| progress.sid = Some(found));
    }
    Ok(())
}

/// `early_find_sid_in`: the string the input's variables hold under the
/// session's name.
fn find_in(req: &mut Request, input: Input, name: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let array = req.variables_mut().input_variables(input)?;
    let found = array
        .find(name)
        .and_then(|value| value.deref().as_str_bytes().map(<[u8]>::to_vec));
    array.give_up(req);
    Ok(found)
}

/// `MULTIPART_EVENT_FILE_START`: at the first file, the progress laid down
/// and the session's request state started with the form's id; then the file's
/// own array.
fn file_start(req: &mut Request, read: i64, name: &[u8], filename: &[u8]) -> Result<(), Thrown> {
    if !tracking(req) {
        return Ok(());
    }
    let first = req
        .state::<State>()
        .progress
        .as_ref()
        .is_some_and(|progress| progress.data.is_none());
    if first {
        let freq = freq(req);
        let start_time = req.environment().request_time();
        with(req, |progress| {
            progress.update_step = if freq >= 0 {
                freq
            } else {
                progress.content_length * -freq / 100
            };
            progress.next_update = 0;
            progress.next_update_time = 0.0;
            progress.data = Some(Data {
                start_time,
                content_length: progress.content_length,
                bytes_processed: read,
                done: false,
                files: Vec::new(),
            });
        });
        session::rinit_without_start(req);
        let sid = req
            .state::<State>()
            .progress
            .as_ref()
            .and_then(|progress| progress.sid.clone());
        let state = req.state::<State>();
        state.id = sid;
        state.send_cookie = false;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(0));
    with(req, |progress| {
        if let Some(data) = progress.data.as_mut() {
            data.files.push(File {
                field_name: name.to_vec(),
                name: filename.to_vec(),
                start_time: now,
                bytes_processed: read,
                ..File::default()
            });
        }
    });
    update(req, false)
}

/// `session.upload_progress.freq` as `OnUpdateRfc1867Freq` keeps it:
/// `int tmp = ZEND_ATOL(...)`, C's `atoi`, a number of bytes, or a
/// percentage of the body as its negative where the text ends in `%`.
fn freq(req: &mut Request) -> i64 {
    let text = ini(req, b"session.upload_progress.freq");
    let tmp = i64::from(atoi(&text));
    if text.last() == Some(&b'%') {
        -tmp
    } else {
        tmp
    }
}

/// `php_session_rfc1867_update`: past the next step and the next moment,
/// or forced, the session started, a `cancel_upload` it holds read, the
/// progress written under its key, and the session written and closed.
fn update(req: &mut Request, force: bool) -> Result<(), Thrown> {
    if !force {
        let min_freq = String::from_utf8_lossy(&ini(req, b"session.upload_progress.min_freq"))
            .trim()
            .parse::<f64>()
            .unwrap_or(0.0);
        let mut due = false;
        with(req, |progress| {
            let read = progress
                .data
                .as_ref()
                .map_or(0, |data| data.bytes_processed);
            if read < progress.next_update {
                return;
            }
            if min_freq > 0.0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |since| since.as_secs_f64());
                if now < progress.next_update_time {
                    return;
                }
                progress.next_update_time = now + min_freq;
            }
            progress.next_update = read + progress.update_step;
            due = true;
        });
        if !due {
            return Ok(());
        }
    }
    updating(req, |req| {
        let key = req
            .state::<State>()
            .progress
            .as_ref()
            .and_then(|progress| progress.key.clone());
        if let (Some(mut vars), Some(key)) = (session_array_mut(req)?, key) {
            let cancel = cancelled(req, vars, &key);
            with(req, |progress| progress.cancel_upload |= cancel);
            let data = rendered(req)?;
            vars.set_str(req, &key, data)?;
        }
        Ok(())
    })
}

/// What `php_session_rfc1867_update` and `php_session_rfc1867_cleanup`
/// share since 8.5.12: `PS(in_rfc1867_update)`, [`Phase::ProgressUpdate`], set while the session is
/// started, `change` made to its variables and the session written and
/// closed; a start that failed, or left the session other than active
/// (a decode refused destroys it), ends there, with nothing written.
///
/// The record is read and written back with every object kept
/// incomplete, a `C:` payload and an `E:` case as they were; one whose
/// object already holds a member that bookkeeping uses is refused and
/// the session destroyed; and the request's program finds no `$_SESSION`:
///
/// ```php
/// $d = __DIR__; $sid = "rfc1867witness"; $f = "$d/sess_$sid";
/// file_put_contents("$d/w.php", '<?php var_dump(array_key_exists("_SESSION", $GLOBALS));');
/// $b = "XYZ";
/// $body = "--$b\r\nContent-Disposition: form-data; name=\"PHPSESSID\"\r\n\r\n$sid\r\n"
///     . "--$b\r\nContent-Disposition: form-data; name=\"PHP_SESSION_UPLOAD_PROGRESS\"\r\n\r\nk\r\n"
///     . "--$b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"f.txt\"\r\n\r\n1\r\n--$b--\r\n";
/// $cmd = [dirname(PHP_BINARY) . "/php-cgi", "-n", "-q", "-d", "session.save_path=$d", "-d", "session.use_cookies=0",
///     "-d", "session.cache_limiter=", "-d", "session.upload_progress.cleanup=0",
///     "-d", "session.upload_progress.freq=1%", "-d", "session.upload_progress.min_freq=0.000000001", "-d", "html_errors=0", "-f", "$d/w.php"];
/// $env = ["REDIRECT_STATUS" => "1", "REQUEST_METHOD" => "POST", "CONTENT_TYPE" => "multipart/form-data; boundary=$b",
///     "CONTENT_LENGTH" => (string) strlen($body), "SCRIPT_FILENAME" => "$d/w.php", "PATH_TRANSLATED" => "$d/w.php", "QUERY_STRING" => ""];
/// foreach (['c|C:4:"Cust":7:{payload}e|a:2:{i:0;E:11:"Suit:Hearts";i:1;r:3;}', 'x|O:8:"stdClass":1:{s:26:"__PHP_Incomplete_Enum_Case";s:3:"foo";}'] as $record) {
///     file_put_contents($f, $record);
///     $p = proc_open($cmd, [0 => ["pipe", "r"], 1 => ["pipe", "w"], 2 => ["pipe", "w"]], $pipes, $d, $env);
///     fwrite($pipes[0], $body); fclose($pipes[0]);
///     echo str_replace($d, "D", stream_get_contents($pipes[1]) . stream_get_contents($pipes[2]));
///     proc_close($p);
///     $after = @file_get_contents($f);
///     var_dump($after === false ? false : substr($after, 0, strlen($record) + 4) === "{$record}uplo");
///     @unlink($f);
/// }
/// unlink("$d/w.php");
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.12
/// Content-type: text/html; charset=UTF-8
///
/// bool(false)
/// bool(true)
/// X-Powered-By: PHP/8.5.12
/// Content-type: text/html; charset=UTF-8
///
///
/// Warning: PHP Request Startup: Failed to decode session object. Session has been destroyed in Unknown on line 0
/// bool(false)
/// bool(false)
/// ```
fn updating(
    req: &mut Request,
    change: impl FnOnce(&mut Request) -> Result<(), Thrown>,
) -> Result<(), Thrown> {
    req.state::<State>().phase = Phase::ProgressUpdate;
    let done = update_started(req, change);
    req.state::<State>().phase = Phase::Startup;
    done
}

/// [`updating`]'s body: the session started, and where it stands
/// active, `change` made and the session written and closed.
fn update_started(
    req: &mut Request,
    change: impl FnOnce(&mut Request) -> Result<(), Thrown>,
) -> Result<(), Thrown> {
    let initialized = session::initialize(req);
    let started = absorb(req, initialized)?.unwrap_or(false);
    if !started || req.state::<State>().status != Status::Active {
        return Ok(());
    }
    change(req)?;
    session::flush(req, true).map(|_| ())
}

/// `php_check_cancel_upload`: the progress array the session holds under
/// the key, with `cancel_upload` set to `true` in it.
fn cancelled(req: &Request, vars: PhpArray, key: &[u8]) -> bool {
    let found = walk::rows(req, vars).find_map(|(had, value)| match had {
        Key::Str(s) if s.as_bytes() == key => Some(value),
        Key::Int(n) if n.to_string().as_bytes() == key => Some(value),
        _ => None,
    });
    found
        .and_then(|value| value.deref().as_array())
        .and_then(|array| array.find(b"cancel_upload"))
        .and_then(|value| value.deref().as_bool())
        .unwrap_or(false)
}

/// The progress as the array php keeps it.
fn rendered(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let taken = req
        .state::<State>()
        .progress
        .as_mut()
        .and_then(|progress| progress.data.take());
    let Some(data) = taken else {
        return Ok(Owner::null());
    };
    let mut array = PhpArray::new(req)?;
    set(req, &mut array, b"start_time", Owner::int(data.start_time))?;
    set(
        req,
        &mut array,
        b"content_length",
        Owner::int(data.content_length),
    )?;
    set(
        req,
        &mut array,
        b"bytes_processed",
        Owner::int(data.bytes_processed),
    )?;
    set(req, &mut array, b"done", Owner::bool(data.done))?;
    let mut files = PhpArray::new(req)?;
    for file in &data.files {
        let mut entry = PhpArray::new(req)?;
        let field_name = string_value(req, &file.field_name)?;
        set(req, &mut entry, b"field_name", field_name)?;
        let name = string_value(req, &file.name)?;
        set(req, &mut entry, b"name", name)?;
        let tmp_name = match &file.tmp_name {
            Some(tmp) => string_value(req, tmp)?,
            None => Owner::null(),
        };
        set(req, &mut entry, b"tmp_name", tmp_name)?;
        set(req, &mut entry, b"error", Owner::int(file.error))?;
        set(req, &mut entry, b"done", Owner::bool(file.done))?;
        set(req, &mut entry, b"start_time", Owner::int(file.start_time))?;
        set(
            req,
            &mut entry,
            b"bytes_processed",
            Owner::int(file.bytes_processed),
        )?;
        files.append(req, entry.into_value())?;
    }
    set(req, &mut array, b"files", files.into_value())?;
    with(req, |progress| progress.data = Some(data));
    Ok(array.into_value())
}

/// `add_assoc_*_ex`.
fn set(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, name, value)
}

/// `MULTIPART_EVENT_END`: the progress taken out of the session under
/// `cleanup`, or marked done and written; the session's request state shut
/// down; the progress given up.
fn end(req: &mut Request, read: i64) -> Result<(), Thrown> {
    if tracking(req) {
        if ini_bool(req, b"session.upload_progress.cleanup") {
            cleanup(req)?;
        } else {
            let started = req
                .state::<State>()
                .progress
                .as_ref()
                .is_some_and(|progress| progress.data.is_some());
            if started {
                with(req, |progress| {
                    if let Some(data) = progress.data.as_mut() {
                        data.done = true;
                        data.bytes_processed = read;
                    }
                });
                update(req, true)?;
            }
        }
        session::rshutdown_globals(req)?;
    }
    req.state::<State>().progress = None;
    // "$_SESSION was populated in incomplete_classes_only mode", so it
    // is no global the program sees (8.5.12).
    req.variables_mut().set_global(b"_SESSION", None);
    Ok(())
}

/// `php_session_rfc1867_cleanup`: the key taken out of the session, and
/// the session written and closed.
fn cleanup(req: &mut Request) -> Result<(), Thrown> {
    updating(req, |req| {
        let key = req
            .state::<State>()
            .progress
            .as_ref()
            .and_then(|progress| progress.key.clone());
        if let (Some(mut vars), Some(key)) = (session_array_mut(req)?, key) {
            let name = PhpString::new(req, &key)?;
            let unset = vars.unset(req, &Key::Str(*name));
            name.give_up(req);
            unset?;
        }
        Ok(())
    })
}
