//! `pdo_sql_parser.re`: the placeholder scanner and what PDO does with
//! the placeholders it finds for a connector that emulates prepares
//! ([`Placeholders::Emulated`]), each one quoted into the query by the
//! connector's quoter; and `rewrite_name_to_position`, which places a
//! parameter bound by name at the position a rewritten query gave it.
//!
//! The scanner is `default_scanner`'s rules in its order: a string in
//! double or single quotes with its doubled quote inside, `::` and `??`
//! repeats, `:name`, `?`, a comment, one special character, and a stretch of
//! anything else; the longest match wins, as re2c's does.
//!
//! [`Placeholders::Emulated`]: crate::connector::Placeholders::Emulated

use php_ext::coerce::{to_long, to_string, truthy};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Emalloc, FreshString, Owner, PhpObject, PhpString, Value, ValueRef};

use crate::bound::{Bound, BoundKey, BoundTable};
use crate::connector::{
    BindKind, BindType, Failure, OnError, ParameterMap, Rewritten, Scanner, SqlState, Token,
};
use crate::error::{self, On};
use crate::stmt;

/// `default_scanner`: the token at `at` and where it ends; `None` at the
/// end of the query or at a NUL, re2c's `PDO_PARSER_EOI`.
pub(crate) fn scan(sql: &[u8], at: usize) -> Option<(Token, usize)> {
    let first = *sql.get(at).filter(|b| **b != 0)?;
    let byte = |i: usize| sql.get(i).copied().filter(|b| *b != 0);
    let run = |from: usize, pick: &dyn Fn(u8) -> bool| {
        let mut end = from;
        while byte(end).is_some_and(pick) {
            end += 1;
        }
        end
    };
    // The longest match: each closing quote is a candidate, and a
    // doubled one may run on.
    let quoted = |quote: u8| {
        let mut best = None;
        let mut end = at + 1;
        loop {
            match byte(end) {
                Some(b) if b == quote => {
                    best = Some(end + 1);
                    if byte(end + 1) != Some(quote) {
                        return best;
                    }
                    end += 2;
                }
                Some(_) => end += 1,
                None => return best,
            }
        }
    };
    let special = |b: u8| matches!(b, b':' | b'?' | b'"' | b'\'' | b'/' | b'-');
    match first {
        b'"' | b'\'' => Some((Token::Text, quoted(first).unwrap_or(at + 1))),
        b':' | b'?' if byte(at + 1) == Some(first) => Some((Token::Text, run(at, &|b| b == first))),
        b':' => {
            let end = run(at + 1, &|b| b.is_ascii_alphanumeric() || b == b'_');
            Some(if end > at + 1 {
                (Token::Bind, end)
            } else {
                (Token::Text, at + 1)
            })
        }
        b'?' => Some((Token::Position, at + 1)),
        b'/' if byte(at + 1) == Some(b'*') => {
            let mut end = at + 2;
            loop {
                match (byte(end), byte(end + 1)) {
                    (Some(b'*'), Some(b'/')) => return Some((Token::Text, end + 2)),
                    (Some(_), _) => end += 1,
                    (None, _) => return Some((Token::Text, at + 1)),
                }
            }
        }
        b'-' if byte(at + 1) == Some(b'-') => Some((Token::Text, run(at, &|b| b != b'\n'))),
        b if special(b) => Some((Token::Text, at + 1)),
        _ => Some((Token::Text, run(at, &|b| !special(b)))),
    }
}

/// `sizeof(struct placeholder)` (`ext/pdo/pdo_sql_parser.re:47`): the
/// position, the length, the quoted string, the number and the link, each
/// a word.
const PLACEHOLDER_SIZE: usize = 40;

/// One placeholder: where it stands, its text, its number, and the
/// string it is replaced by, `plc->quoted`, which holds one owner until
/// the query is built. `block` is the `emalloc(sizeof(*plc))` php keeps
/// it in from the scan to `clean_up`.
struct Placeholder {
    at: usize,
    len: usize,
    bindno: i64,
    quoted: Option<Owner<PhpString>>,
    block: Emalloc,
}

impl Placeholder {
    /// The text that takes the placeholder's place: what was quoted, or
    /// the placeholder as written.
    fn text<'a>(&'a self, sql: &'a [u8]) -> &'a [u8] {
        self.quoted
            .as_deref()
            .map_or(&sql[self.at..self.at + self.len], PhpString::as_bytes)
    }
}

/// What `pdo_parse_params` answers: the query unchanged (`0`), the
/// query as PDO rewrote it (`1`), or a failure recorded on the
/// statement (`-1`).
pub(crate) enum Parsed {
    Unchanged,
    /// The query PDO made, `zend_string_alloc`'d on the request's heap.
    Rewritten(Owner<PhpString>),
    Failed,
}

/// `pdo_parse_params` for a connector that emulates prepares: every
/// placeholder found, the mix of the two kinds and a count that does not
/// match refused, and each bound value quoted into its place as its type
/// says. Each placeholder and its quoted string are held until
/// `clean_up`, after the query is built, so the query's allocation peaks
/// with them as php's does.
///
/// ```php
/// $q = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '', [PDO::ATTR_EMULATE_PREPARES => true]);
/// foreach ([1, 30, 113] as $n) {
///     $s = $q->prepare('DO ?, ?');
///     $s->bindValue(1, str_repeat('a', $n));
///     $s->bindValue(2, str_repeat('b', $n));
///     memory_reset_peak_usage(); $m = memory_get_usage(); $s->execute();
///     echo $n, ' ', memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, "\n";
///     unset($s);
/// }
/// $s = $q->prepare('DO ?, ?, ?');
/// $s->bindValue(1, null); $s->bindValue(2, true, PDO::PARAM_BOOL); $s->bindValue(3, 1234, PDO::PARAM_INT);
/// memory_reset_peak_usage(); $m = memory_get_usage(); $s->execute();
/// echo memory_get_peak_usage() - $m, ' ', memory_get_usage() - $m, "\n";
/// ```
/// ```expect mysql regime=jit-off
/// 1 200 88
/// 30 304 112
/// 113 720 336
/// 200 64
/// ```
pub(super) fn parse_params(
    req: &mut Request,
    object: PhpObject,
    dbh: PhpObject,
    sql: &[u8],
) -> Result<Parsed, Thrown> {
    let mut placeholders = Vec::new();
    let parsed = parse_into(req, object, dbh, sql, &mut placeholders);
    // `clean_up`: each quoted string released and each placeholder freed,
    // in the order of the list.
    for placeholder in placeholders {
        if let Some(quoted) = placeholder.quoted {
            quoted.give_up(req);
        }
        placeholder.block.free(req);
    }
    parsed
}

/// [`parse_params`] up to `clean_up`, the placeholders it found left in
/// `placeholders` for it to free.
fn parse_into(
    req: &mut Request,
    object: PhpObject,
    dbh: PhpObject,
    sql: &[u8],
    placeholders: &mut Vec<Placeholder>,
) -> Result<Parsed, Thrown> {
    let scanner = crate::dbh::with_connection(req, dbh, |_, conn| Ok(conn.scanner()))?;
    let scan = scanner.unwrap_or(scan);
    let (mut named, mut positional) = (false, false);
    let mut at = 0;
    let mut bindno = 0;
    while let Some((token, end)) = scan(sql, at) {
        let placed = match token {
            Token::Bind if at > 0 && sql[at - 1].is_ascii_alphanumeric() => false,
            Token::Bind => {
                named = true;
                true
            }
            Token::Position => {
                positional = true;
                true
            }
            Token::Text => false,
        };
        if placed {
            placeholders.push(Placeholder {
                at,
                len: end - at,
                bindno,
                quoted: None,
                block: Emalloc::new(req, PLACEHOLDER_SIZE)?,
            });
            bindno += 1;
        }
        at = end;
    }
    let fail = |req: &mut Request, supp: &str| -> Result<Parsed, Thrown> {
        error::raise_impl(req, dbh, On::Stmt(object), SqlState(*b"HY093"), Some(supp))?;
        Ok(Parsed::Failed)
    };
    if named && positional {
        return fail(req, "mixed named and positional parameters");
    }
    // Each entry's key, type and value, a view: the entry holds its owner.
    let params: Vec<Seen> = stmt::read(req, object, |s| {
        s.params
            .iter()
            .flat_map(BoundTable::iter)
            .map(|b| (b.key(), b.ty, *b.value))
            .collect()
    })
    .unwrap_or_default();
    let count = i64::try_from(params.len()).unwrap_or(i64::MAX);
    if !params.is_empty() && bindno != count {
        let every = !positional
            && bindno > count
            && placeholders.iter().all(|p| {
                find(&params, &BoundKey::Name(sql[p.at..p.at + p.len].to_vec())).is_some()
            });
        if !every {
            return fail(
                req,
                "number of bound variables does not match number of tokens",
            );
        }
    }
    if placeholders.is_empty() {
        return Ok(Parsed::Unchanged);
    }
    if !params.is_empty() {
        for placeholder in placeholders.iter_mut() {
            let key = if positional {
                BoundKey::Position(placeholder.bindno)
            } else {
                BoundKey::Name(sql[placeholder.at..placeholder.at + placeholder.len].to_vec())
            };
            let Some(bound) = find(&params, &key) else {
                return fail(req, "parameter was not defined");
            };
            match quote(req, object, dbh, (bound.1, bound.2))? {
                Some(quoted) => placeholder.quoted = Some(quoted),
                None => return Ok(Parsed::Failed),
            }
        }
    }
    Ok(Parsed::Rewritten(build(req, sql, placeholders)?))
}

/// The `rewrite:` arm: `newbuffer_len`, then the query built into
/// `zend_string_alloc`'s string of that length, each placeholder's text
/// kept or, where values are `quoted`, its quoted value in its place, and
/// `ZSTR_LEN` set to what was written. Query generation counts the query
/// whole and adds each quoted value without taking the placeholder's own
/// text off (`ext/pdo/pdo_sql_parser.re:210`, `:325`), so the string
/// keeps the bytes of every placeholder past its end: `DO ?` with a
/// 4-digit integer is a 7-byte query in the 40-byte bin, which 7 bytes
/// alone would not reach, and the emulated statement holds it while it
/// stands:
///
/// ```php
/// $q = new PDO('mysql:host=127.0.0.1;dbname=test', 'root', '');
/// foreach ([3, 4, 5, 12] as $n) {
///     $s = $q->prepare('DO ?');
///     $s->bindValue(1, (int) str_repeat('1', $n), PDO::PARAM_INT);
///     $m = memory_get_usage(); $s->execute(); $d = memory_get_usage() - $m;
///     echo $n, ' ', $d, "\n";
///     unset($s);
/// }
/// ```
/// ```expect mysql regime=jit-off
/// 3 48
/// 4 56
/// 5 56
/// 12 64
/// ```
fn build(
    req: &mut Request,
    sql: &[u8],
    placeholders: &[Placeholder],
) -> Result<Owner<PhpString>, Thrown> {
    let newbuffer_len = placeholders
        .iter()
        .filter_map(|p| p.quoted.as_deref())
        .fold(sql.len(), |len, quoted| len + quoted.len());
    let mut out = FreshString::new(req, newbuffer_len)?;
    let to = out.bytes_mut();
    let mut put = 0;
    let mut copy = |bytes: &[u8]| {
        to[put..put + bytes.len()].copy_from_slice(bytes);
        put += bytes.len();
    };
    let mut from = 0;
    for placeholder in placeholders {
        copy(&sql[from..placeholder.at]);
        copy(placeholder.text(sql));
        from = placeholder.at + placeholder.len;
    }
    copy(&sql[from..]);
    out.set_len(put);
    Ok(out.finish())
}

/// The entry held under `key`.
fn find<'a>(params: &'a [Seen], key: &BoundKey) -> Option<&'a Seen> {
    params.iter().find(|b| &b.0 == key)
}

/// A bound entry as the parser reads it: its key, its type and its value.
type Seen = (BoundKey, BindType, Value);

/// One bound value as the string that stands for it in the query, one
/// owner the placeholder's: `NULL` (`ZSTR_KNOWN(ZEND_STR_NULL)`), `1` or
/// `0` (`ZSTR_CHAR`), an integer (`zend_long_to_str`, interned for one
/// digit), or the connector's quoting of its string; `None` where the
/// quoter failed, its state copied onto the statement. The string a
/// value converts to (`zval_try_get_string`) or a stream's contents
/// (`php_stream_copy_to_mem`) is held while the quoter runs and released
/// after it, as php releases `buf`. Where the quoter fails, php's `goto
/// clean_up` skips that release (`ext/pdo/pdo_sql_parser.re:303-307`),
/// which leaks `buf` until the request ends. No bundled connector that
/// emulates prepares has a quoter that fails, and the engine releases it.
fn quote(
    req: &mut Request,
    object: PhpObject,
    dbh: PhpObject,
    (ty, value): (BindType, Value),
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let value = value.deref();
    let buf = match (ty.kind(), value.as_ref()) {
        (BindKind::Lob, ValueRef::Resource(_)) => {
            let Some(stream) = php_ext::request::Stream::from_value(req, &value) else {
                error::raise_impl(
                    req,
                    dbh,
                    On::Stmt(object),
                    SqlState(*b"HY105"),
                    Some("Expected a stream resource"),
                )?;
                return Ok(None);
            };
            stream.copy_to_mem(req, None)?
        }
        (_, ValueRef::Null) | (BindKind::Null, _) => {
            let null = PhpString::interned(req, b"NULL")?;
            return Ok(Some(Owner::take(req, null)));
        }
        (BindKind::Bool, _) => {
            let digit: &[u8] = if truthy(req, &value)? { b"1" } else { b"0" };
            return PhpString::new(req, digit).map(Some);
        }
        (BindKind::Int, _) => {
            let long = to_long(req, &value)?;
            return PhpString::new(req, long.to_string().as_bytes()).map(Some);
        }
        _ => to_string(req, &value)?,
    };
    let mode = crate::dbh::state(req, dbh).map_or(OnError::Throw, |d| d.error_mode);
    let quoted = crate::dbh::with_connection(req, dbh, |req, conn| {
        Ok(conn.quote(req, buf.as_bytes(), ty, mode))
    });
    buf.give_up(req);
    match quoted? {
        Ok(quoted) => Ok(Some(quoted)),
        Err(failure) => {
            error::record(req, dbh, On::Dbh, failure.cause)?;
            let state = crate::dbh::state(req, dbh).and_then(|d| d.error_code);
            if let Some(state) = state {
                error::set_state(req, dbh, On::Stmt(object), state);
            }
            match failure.thrown {
                Some(thrown) => Err(thrown),
                None => Ok(None),
            }
        }
    }
}

/// `rewrite_name_to_position`: where the query was rewritten, a
/// parameter bound by name placed at the position its placeholder took,
/// and one bound by position given the name it stood for; `false` where
/// the name or the position is none of the query's.
pub(super) fn name_to_position(
    req: &mut Request,
    object: PhpObject,
    bound: &mut Bound,
) -> Result<bool, Thrown> {
    let named = bound.name_bytes().map(<[u8]>::to_vec);
    // A view of the map's string, or the position a name took: no PHP
    // code runs between the read and the owner taken of it.
    let found = stmt::read(req, object, |s| {
        s.param_map.as_ref().map(|map| match &named {
            None => map.name(bound.position).map(Found::Name),
            Some(name) => map.position(name).map(Found::Position),
        })
    })
    .flatten();
    let Some(found) = found else {
        return Ok(true);
    };
    let Some(dbh) = stmt::dbh_of(req, object) else {
        return Ok(true);
    };
    match found {
        // `param->name = zend_string_copy(name)`: one more owner of the
        // map's string.
        Some(Found::Name(name)) => {
            bound.name = Some(Owner::take(req, name));
            Ok(true)
        }
        Some(Found::Position(_)) if bound.position >= 0 => {
            error::raise_impl(
                req,
                dbh,
                On::Stmt(object),
                SqlState(*b"IM001"),
                Some(
                    "PDO refuses to handle repeating the same :named parameter for multiple positions with this driver, as it might be unsafe to do so.  Consider using a separate name for each parameter instead",
                ),
            )?;
            // php 8.5.12 answers `false`; 8.5.9's `-1` read as true.
            Ok(false)
        }
        Some(Found::Position(position)) => {
            bound.position = i64::try_from(position).unwrap_or(-1);
            Ok(true)
        }
        None => {
            error::raise_impl(
                req,
                dbh,
                On::Stmt(object),
                SqlState(*b"HY093"),
                Some("parameter was not defined"),
            )?;
            Ok(false)
        }
    }
}

/// What `rewrite_name_to_position` finds in the map: the name a
/// position stood for, or the position a name took.
enum Found {
    Name(PhpString),
    Position(usize),
}

/// The positional rewrite of `pdo_parse_params`, for a connector that
/// takes `?` natively: each `:name` (not after a letter or digit) made a
/// `?`, its number mapped to its name; `None` where there is none; named
/// and positional together refused with php's `HY093`.
pub(crate) fn rewrite_named(
    req: &mut Request,
    sql: &[u8],
    scanner: Option<Scanner>,
) -> Result<Option<Rewritten>, Failure> {
    let scan = scanner.unwrap_or(scan);
    let (mut named, mut positional) = (false, false);
    let mut found = Vec::new();
    let mut at = 0;
    while let Some((token, end)) = scan(sql, at) {
        match token {
            Token::Bind if at > 0 && sql[at - 1].is_ascii_alphanumeric() => {}
            Token::Bind => {
                named = true;
                found.push((at, end));
            }
            Token::Position => {
                positional = true;
                found.push((at, end));
            }
            Token::Text => {}
        }
        at = end;
    }
    if named && positional {
        return Err(Failure::raised(
            SqlState(*b"HY093"),
            "mixed named and positional parameters",
        ));
    }
    if !named {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(sql.len());
    let mut map = ParameterMap::new(req)?;
    let mut from = 0;
    for (bindno, (start, end)) in found.into_iter().enumerate() {
        out.extend_from_slice(&sql[from..start]);
        out.push(b'?');
        let bindno = i64::try_from(bindno).unwrap_or(i64::MAX);
        if let Err(thrown) = map.insert(req, bindno, &sql[start..end]) {
            map.give_up(req);
            return Err(thrown.into());
        }
        from = end;
    }
    out.extend_from_slice(&sql[from..]);
    Ok(Some(Rewritten {
        sql: out,
        parameter_map: map,
    }))
}
