//! Watchpoints (`Z/sapi/phpdbg/phpdbg_watch.c`), compared between steps
//! (#1308): php protects the pages a watched value lives on and, once a
//! write faulted, compares the value it backed up with what the page
//! holds at the next opline boundary; this host reads each watched value
//! at every boundary and compares it with the copy it kept, by the same
//! rules. A zval that changed its type, its scalar, or the string, array,
//! object or reference it points to is reported as php reports a
//! `WATCH_ON_ZVAL`; a string changed in place as a `WATCH_ON_STR`; an
//! array that grew or shrank as a `WATCH_ON_HASHTABLE`.

use php_sapi::tier::{self, FrameRef, Key, Request, Shown};

use crate::cmd::Param;
use crate::info::flat;
use crate::out;
use crate::prompt::{Here, Ret};
use crate::session::{self, Session};

/// How deep a watched value is copied.
const DEPTH: u32 = 16;

/// What a watch observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `watch $x`: the zval, and a string's bytes.
    Simple,
    /// `watch recursive $x`: the zval, and everything it holds.
    Recursive,
    /// `watch array $x`: the array's element count.
    Array,
}

/// One watch.
#[derive(Debug, Clone)]
struct Watch {
    id: u32,
    kind: Kind,
    /// The frame whose variable it is.
    frame: FrameRef,
    variable: Vec<u8>,
    /// The keys past the variable, as written.
    path: Vec<Vec<u8>>,
    /// The name it is reported under, `$a[0]`, `$a[]`.
    name: Vec<u8>,
    /// What it held at the last boundary.
    old: Shown,
}

/// The watchpoints of the session.
#[derive(Debug, Default, Clone)]
pub struct Watches {
    list: Vec<Watch>,
    next: u32,
}

/// A path as written after its variable: each key, `None` for an empty
/// `[]`, which stands for every element.
type Keys = Vec<Option<Vec<u8>>>;

/// `phpdbg_parse_variable`'s reading of a path: the variable's name and
/// the keys after it; `None` for input it calls malformed.
fn parse(input: &[u8]) -> Option<(Vec<u8>, Keys)> {
    if input.len() < 2 || input[0] != b'$' {
        return None;
    }
    let name_end = input[1..]
        .iter()
        .position(|b| matches!(b, b'[' | b'-' | b'>' | b']'))
        .map_or(input.len(), |p| p + 1);
    let name = input[1..name_end].to_vec();
    if name.is_empty() {
        return None;
    }
    let mut parts: Keys = Vec::new();
    let mut at = name_end;
    while at < input.len() {
        match input[at] {
            b'[' => {
                let close = input[at..].iter().position(|b| *b == b']')? + at;
                let key = &input[at + 1..close];
                parts.push((!key.is_empty()).then(|| key.to_vec()));
                at = close + 1;
            }
            b'-' if input.get(at + 1) == Some(&b'>') => {
                let rest = &input[at + 2..];
                let end = rest
                    .iter()
                    .position(|b| matches!(b, b'[' | b'-'))
                    .unwrap_or(rest.len());
                if end == 0 {
                    return None;
                }
                parts.push(Some(rest[..end].to_vec()));
                at += 2 + end;
            }
            _ => return None,
        }
    }
    Some((name, parts))
}

/// A key as `zend_symtable_str_find` reads it: a canonical decimal as the
/// integer.
fn key_matches(key: &Key, written: &[u8]) -> bool {
    match key {
        Key::Str(s) => s.as_slice() == written,
        Key::Int(n) => n.to_string().as_bytes() == written,
    }
}

fn deref(value: &Shown) -> &Shown {
    match value {
        Shown::Reference { value, .. } => value,
        other => other,
    }
}

/// The value down `path` from `root`, following references.
fn walk<'a>(root: &'a Shown, path: &[Vec<u8>]) -> Option<&'a Shown> {
    let mut at = root;
    for key in path {
        let Shown::Array { entries, .. } = deref(at) else {
            return None;
        };
        at = entries
            .iter()
            .find(|(k, _)| key_matches(k, key))
            .map(|(_, v)| v)?;
    }
    Some(at)
}

/// What a watch reads now: none where its path reaches nothing.
fn read(req: &mut Request, watch: &Watch) -> Option<Shown> {
    let root = tier::variable(req, watch.frame, &watch.variable, DEPTH)?;
    walk(&root, &watch.path).cloned()
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Every path `parts` names under `root`, an empty key expanded over the
/// elements it stands at, each with the name it is reported under.
fn expand(root: &Shown, name: Vec<u8>, parts: &[Option<Vec<u8>>]) -> Vec<(Vec<Vec<u8>>, Vec<u8>)> {
    let mut paths: Vec<(Vec<Vec<u8>>, Vec<u8>)> = vec![(Vec::new(), name)];
    for part in parts {
        let mut next = Vec::new();
        for (path, name) in paths {
            match part {
                Some(key) => {
                    let mut path = path;
                    path.push(key.clone());
                    next.push((path, [name.as_slice(), b"[", key, b"]"].concat()));
                }
                None => {
                    if let Some(Shown::Array { entries, .. }) = walk(root, &path).map(deref) {
                        for (key, _) in entries {
                            let written = match key {
                                Key::Int(n) => n.to_string().into_bytes(),
                                Key::Str(s) => s.clone(),
                            };
                            let mut path = path.clone();
                            path.push(written.clone());
                            next.push((path, [name.as_slice(), b"[", &written, b"]"].concat()));
                        }
                    }
                }
            }
        }
        paths = next;
    }
    paths
}

/// A watch set on every path `input` names in the frame the prompt
/// stands in, as `phpdbg_watchpoint_parse_symtables` sets them; none
/// where nothing is executing, `phpdbg_rebuild_symtable`'s refusal.
fn add(s: &mut Session, here: &mut Here<'_>, input: &[u8], kind: Kind) {
    let Some((req, frame)) = here.standing() else {
        out::error(b"No active op array!");
        return;
    };
    let Some((variable, parts)) = parse(input) else {
        out::error(b"Malformed input");
        return;
    };
    let Some(root) = tier::variable(req, frame, &variable, DEPTH) else {
        out::error(format!("{} is undefined", lossy(input)).as_bytes());
        return;
    };
    let named = [b"$".as_slice(), &variable].concat();
    for (path, name) in expand(&root, named, &parts) {
        let Some(old) = walk(&root, &path).cloned() else {
            out::error(format!("{} is undefined", lossy(&name)).as_bytes());
            continue;
        };
        let name = if kind == Kind::Array {
            if !matches!(deref(&old), Shown::Array { .. }) {
                continue;
            }
            [name.as_slice(), b"[]"].concat()
        } else {
            name
        };
        let id = s.watches.next;
        s.watches.next += 1;
        out::notice(
            format!(
                "Added{} watchpoint #{id} for {}",
                if kind == Kind::Recursive {
                    " recursive"
                } else {
                    ""
                },
                lossy(&name)
            )
            .as_bytes(),
        );
        s.watches.list.push(Watch {
            id,
            kind,
            frame,
            variable: variable.clone(),
            path,
            name,
            old,
        });
    }
}

/// `PHPDBG_COMMAND(watch)`: a simple watch, or the list with nothing.
pub fn watch(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    match params.first() {
        None => list(s, here, params),
        Some(Param::Str(input)) => {
            let input = input.clone();
            add(s, here, &input, Kind::Simple);
            Ret::Success
        }
        Some(_) => Ret::Success,
    }
}

/// `PHPDBG_WATCH(array)`.
pub fn array(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    if let Some(Param::Str(input)) = params.first() {
        let input = input.clone();
        add(s, here, &input, Kind::Array);
    }
    Ret::Success
}

/// `PHPDBG_WATCH(recursive)`.
pub fn recursive(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    if let Some(Param::Str(input)) = params.first() {
        let input = input.clone();
        add(s, here, &input, Kind::Recursive);
    }
    Ret::Success
}

/// `PHPDBG_WATCH(delete)`.
pub fn delete(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    if let Some(Param::Num(id)) = params.first() {
        match s.watches.list.iter().position(|w| i64::from(w.id) == *id) {
            Some(at) => {
                s.watches.list.remove(at);
                out::notice(format!("Removed watchpoint {id}").as_bytes());
            }
            None => out::error(b"Nothing was deleted, no corresponding watchpoint found"),
        }
    }
    Ret::Success
}

/// `phpdbg_list_watchpoints`.
pub fn list(s: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    for watch in &s.watches.list {
        out::writeln(
            format!(
                "{} ({}, {})",
                lossy(&watch.name),
                if watch.kind == Kind::Array {
                    "array"
                } else {
                    "variable"
                },
                if watch.kind == Kind::Recursive {
                    "recursive"
                } else {
                    "simple"
                }
            )
            .as_bytes(),
        );
    }
    Ret::Success
}

/// Whether two zvals differ as php's `WATCH_ON_ZVAL` compares them: the
/// type, the scalar, the place a counted value points to.
// One arm per type, whose scalars are of different Rust types.
#[allow(clippy::match_same_arms)]
fn zval_differs(old: &Shown, new: &Shown) -> bool {
    match (old, new) {
        (Shown::Undef, Shown::Undef) | (Shown::Null, Shown::Null) => false,
        (Shown::Bool(a), Shown::Bool(b)) => a != b,
        (Shown::Int(a), Shown::Int(b)) => a != b,
        (Shown::Float(a), Shown::Float(b)) => a.to_bits() != b.to_bits(),
        (
            Shown::Str {
                id: a,
                counted: ca,
                bytes: ba,
            },
            Shown::Str {
                id: b,
                counted: cb,
                bytes: bb,
            },
        ) => {
            if *ca && *cb {
                a != b
            } else {
                ba != bb
            }
        }
        (Shown::Array { id: a, .. }, Shown::Array { id: b, .. })
        | (Shown::Object { id: a, .. }, Shown::Object { id: b, .. })
        | (Shown::Reference { id: a, .. }, Shown::Reference { id: b, .. }) => a != b,
        (Shown::Resource { handle: a }, Shown::Resource { handle: b }) => a != b,
        _ => true,
    }
}

/// Whether the report must stop: what it wrote ran PHP code, a header
/// callback, that ended the request, `quit` at a prompt inside it.
fn ended(req: &mut Request) -> bool {
    tier::exiting(req) || session::with(|s| s.stopped.is_some()).unwrap_or(true)
}

/// `phpdbg_print_watch_diff` for a zval: the labels to phpdbg's own
/// stream, the values through the request's output as
/// `zend_print_flat_zval_r` writes them with `zend_write`, so the first
/// byte sends the headers and a header callback runs, under the
/// debugger, before the rest is written; `false` where that ended the
/// request.
fn report_zval(req: &mut Request, name: &[u8], old: &Shown, new: &Shown) -> bool {
    out::notice(format!("Breaking on watchpoint {}", lossy(name)).as_bytes());
    if old.counted() {
        out::writeln(b"Old value inaccessible or destroyed");
    } else {
        out::out(b"Old value: ");
        out::interruptible(|| req.write(&flat(old)));
        if ended(req) {
            return false;
        }
        out::out(b"\n");
    }
    let reference = matches!(new, Shown::Reference { .. });
    out::out(format!("New value{}: ", if reference { " (reference)" } else { "" }).as_bytes());
    out::interruptible(|| req.write(&flat(new)));
    if ended(req) {
        return false;
    }
    out::out(b"\n");
    true
}

/// `phpdbg_print_watch_diff` for a string changed in place, its bytes
/// written with `zend_write` as the zval's are.
fn report_str(req: &mut Request, name: &[u8], old: &[u8], new: &[u8]) -> bool {
    out::notice(format!("Breaking on watchpoint {}", lossy(name)).as_bytes());
    out::out(b"Old value: ");
    out::interruptible(|| req.write(old));
    if ended(req) {
        return false;
    }
    out::out(b"\n");
    out::out(b"New value: ");
    out::interruptible(|| req.write(new));
    if ended(req) {
        return false;
    }
    out::out(b"\n");
    true
}

/// A string reached through the same references and the same string,
/// its bytes changed: the `WATCH_ON_STR` php keeps beside the zval's.
fn str_changed<'a>(old: &'a Shown, new: &'a Shown) -> Option<(&'a [u8], &'a [u8])> {
    match (old, new) {
        (Shown::Reference { value: a, .. }, Shown::Reference { value: b, .. }) => str_changed(a, b),
        (
            Shown::Str {
                id: a, bytes: ba, ..
            },
            Shown::Str {
                id: b, bytes: bb, ..
            },
        ) if a == b && ba != bb => Some((ba, bb)),
        _ => None,
    }
}

/// The copy the session holds of watch `id` now: what a check the report
/// re-entered left there, or none where it is gone.
fn held(id: u32) -> Option<Shown> {
    session::with(|s| {
        s.watches
            .list
            .iter()
            .find(|watch| watch.id == id)
            .map(|watch| watch.old.clone())
    })
    .flatten()
}

/// `phpdbg_print_changed_zvals`: every watch read again and compared
/// with what it held, each change reported and then kept as what it
/// holds; whether any was, `PHPDBG_G(watchpoint_hit)` read and cleared.
/// The session stands in its cell throughout, so a header callback a
/// report's first byte runs is checked by the debugger too, and finds
/// the same change, as php's does; the hit it takes is cleared before
/// this check reads it.
pub fn changed(req: &mut Request) -> bool {
    let Some(list) = session::with(|s| s.watches.list.clone()) else {
        return false;
    };
    for watch in list {
        let Some(new) = read(req, &watch) else {
            out::notice(
                format!(
                    "{} has been removed, removing watchpoint{}",
                    lossy(&watch.name),
                    if watch.kind == Kind::Recursive {
                        " recursively"
                    } else {
                        ""
                    }
                )
                .as_bytes(),
            );
            session::with(|s| s.watches.list.retain(|w| w.id != watch.id));
            continue;
        };
        let Some(old) = held(watch.id) else {
            continue;
        };
        let reported = match watch.kind {
            Kind::Array => {
                let count = |v: &Shown| match deref(v) {
                    Shown::Array { len, .. } => Some(*len),
                    _ => None,
                };
                if let (Some(old), Some(new)) = (count(&old), count(&new))
                    && old != new
                {
                    session::with(|s| s.watch_hit = true);
                    out::notice(
                        format!("Breaking on watchpoint {}", lossy(&watch.name)).as_bytes(),
                    );
                    let message = if old > new {
                        format!("{} elements were removed from the array", old - new)
                    } else {
                        format!("{} elements were added to the array", new - old)
                    };
                    out::writeln(message.as_bytes());
                }
                true
            }
            Kind::Simple | Kind::Recursive => {
                if zval_differs(&old, &new) {
                    session::with(|s| s.watch_hit = true);
                    report_zval(req, &watch.name, &old, &new)
                } else if let Some((was, now)) = str_changed(&old, &new) {
                    session::with(|s| s.watch_hit = true);
                    report_str(req, &watch.name, was, now)
                } else {
                    true
                }
            }
        };
        if !reported {
            return false;
        }
        session::with(|s| {
            if let Some(kept) = s.watches.list.iter_mut().find(|w| w.id == watch.id) {
                kept.old = new;
            }
        });
    }
    session::with(|s| std::mem::take(&mut s.watch_hit)).unwrap_or(false)
}

/// The watchpoints left when a run ends, each reported removed as the
/// request's teardown frees what it watched: the elements of arrays as
/// the symbol table is destroyed, then the variables themselves.
pub fn removed_at_end(s: &mut Session) {
    let list = std::mem::take(&mut s.watches.list);
    let (elements, variables): (Vec<&Watch>, Vec<&Watch>) =
        list.iter().partition(|watch| !watch.path.is_empty());
    for watch in elements.iter().rev().chain(variables.iter().rev()) {
        out::notice(
            format!(
                "{} has been removed, removing watchpoint{}",
                lossy(&watch.name),
                if watch.kind == Kind::Recursive {
                    " recursively"
                } else {
                    ""
                }
            )
            .as_bytes(),
        );
    }
}
