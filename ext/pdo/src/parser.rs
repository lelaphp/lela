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
use php_ext::values::{Key, PhpObject, ValueRef};

use crate::connector::{BindKind, Failure, OnError, Rewritten, Scanner, SqlState, Token};
use crate::dbh::Dbh;
use crate::error::{self, On};
use crate::stmt::{self, Bound, BoundKey};

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

/// One placeholder: where it stands, its text, its number, and the text
/// it is replaced by.
struct Placeholder {
    at: usize,
    len: usize,
    bindno: i64,
    quoted: Vec<u8>,
}

/// What `pdo_parse_params` answers: the query unchanged (`0`), the
/// query as PDO rewrote it (`1`), or a failure recorded on the
/// statement (`-1`).
pub(crate) enum Parsed {
    Unchanged,
    Rewritten(Vec<u8>),
    Failed,
}

/// `pdo_parse_params` for a connector that emulates prepares: every
/// placeholder found, the mix of the two kinds and a count that does not
/// match refused, and each bound value quoted into its place as its type
/// says.
pub(super) fn parse_params(
    req: &mut Request,
    object: PhpObject,
    dbh: PhpObject,
    sql: &[u8],
) -> Result<Parsed, Thrown> {
    let scanner = crate::dbh::with_connection(req, dbh, |_, conn| Ok(conn.scanner()))?;
    let scan = scanner.unwrap_or(scan);
    let mut placeholders = Vec::new();
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
                quoted: Vec::new(),
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
    let params: Vec<Bound> = stmt::read(req, object, |s| s.params.clone()).unwrap_or_default();
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
        for placeholder in &mut placeholders {
            let key = if positional {
                BoundKey::Position(placeholder.bindno)
            } else {
                BoundKey::Name(sql[placeholder.at..placeholder.at + placeholder.len].to_vec())
            };
            let Some(bound) = find(&params, &key) else {
                return fail(req, "parameter was not defined");
            };
            match quote(req, object, dbh, bound)? {
                Some(quoted) => placeholder.quoted = quoted,
                None => return Ok(Parsed::Failed),
            }
        }
    }
    let mut out = Vec::with_capacity(sql.len());
    let mut from = 0;
    for placeholder in &placeholders {
        out.extend_from_slice(&sql[from..placeholder.at]);
        if params.is_empty() {
            out.extend_from_slice(&sql[placeholder.at..placeholder.at + placeholder.len]);
        } else {
            out.extend_from_slice(&placeholder.quoted);
        }
        from = placeholder.at + placeholder.len;
    }
    out.extend_from_slice(&sql[from..]);
    Ok(Parsed::Rewritten(out))
}

/// The entry held under `key`.
fn find<'a>(params: &'a [Bound], key: &BoundKey) -> Option<&'a Bound> {
    params.iter().find(|b| &b.key == key)
}

/// One bound value as the query text it stands for: `NULL`, `1` or `0`,
/// an integer, or the connector's quoting of its string; `None` where the
/// quoter failed, its state copied onto the statement.
fn quote(
    req: &mut Request,
    object: PhpObject,
    dbh: PhpObject,
    bound: &Bound,
) -> Result<Option<Vec<u8>>, Thrown> {
    let value = bound.value.deref();
    let ty = bound.ty;
    let text = match (ty.kind(), value.as_ref()) {
        (BindKind::Lob, ValueRef::Resource(_)) => {
            if let Some(stream) = php_ext::request::Stream::from_value(req, &value) {
                stream.copy_to_mem(req, None)?.as_bytes().to_vec()
            } else {
                error::raise_impl(
                    req,
                    dbh,
                    On::Stmt(object),
                    SqlState(*b"HY105"),
                    Some("Expected a stream resource"),
                )?;
                return Ok(None);
            }
        }
        (_, ValueRef::Null) | (BindKind::Null, _) => return Ok(Some(b"NULL".to_vec())),
        (BindKind::Bool, _) => {
            return Ok(Some(if truthy(req, &value) {
                b"1".to_vec()
            } else {
                b"0".to_vec()
            }));
        }
        (BindKind::Int, _) => return Ok(Some(to_long(req, &value)?.to_string().into_bytes())),
        _ => to_string(req, &value)?.as_bytes().to_vec(),
    };
    let mode = dbh
        .native::<Dbh>(req)
        .map_or(OnError::Throw, |d| d.error_mode);
    let quoted =
        crate::dbh::with_connection(req, dbh, |req, conn| Ok(conn.quote(req, &text, ty, mode)))?;
    match quoted {
        Ok(quoted) => Ok(Some(quoted)),
        Err(failure) => {
            error::record(req, dbh, On::Dbh, failure.cause)?;
            let state = dbh.native::<Dbh>(req).and_then(|d| d.error_code);
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
    let Some(map) = stmt::read(req, object, |s| s.param_map.clone()).flatten() else {
        return Ok(true);
    };
    let Some(dbh) = stmt::dbh_of(req, object) else {
        return Ok(true);
    };
    let undefined = |req: &mut Request| -> Result<bool, Thrown> {
        error::raise_impl(
            req,
            dbh,
            On::Stmt(object),
            SqlState(*b"HY093"),
            Some("parameter was not defined"),
        )?;
        Ok(false)
    };
    let Some(name) = bound.name.clone() else {
        return match map.iter().find(|(key, _)| *key == Key::Int(bound.position)) {
            Some((_, name)) => {
                bound.name = Some(name.clone());
                Ok(true)
            }
            None => undefined(req),
        };
    };
    for (position, (_, mapped)) in map.iter().enumerate() {
        if *mapped != name {
            continue;
        }
        if bound.position >= 0 {
            error::raise_impl(
                req,
                dbh,
                On::Stmt(object),
                SqlState(*b"IM001"),
                Some(
                    "PDO refuses to handle repeating the same :named parameter for multiple positions with this driver, as it might be unsafe to do so.  Consider using a separate name for each parameter instead",
                ),
            )?;
            return Ok(true);
        }
        bound.position = i64::try_from(position).unwrap_or(-1);
        return Ok(true);
    }
    undefined(req)
}

/// The positional rewrite of `pdo_parse_params`, for a connector that
/// takes `?` natively: each `:name` (not after a letter or digit) made a
/// `?`, its number mapped to its name; `None` where there is none; named
/// and positional together refused with php's `HY093`.
pub(crate) fn rewrite_named(
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
    let mut map = Vec::with_capacity(found.len());
    let mut from = 0;
    for (bindno, (start, end)) in found.into_iter().enumerate() {
        out.extend_from_slice(&sql[from..start]);
        out.push(b'?');
        map.push((
            i64::try_from(bindno).unwrap_or(i64::MAX),
            sql[start..end].to_vec(),
        ));
        from = end;
    }
    out.extend_from_slice(&sql[from..]);
    Ok(Some(Rewritten {
        sql: out,
        parameter_map: map,
    }))
}
