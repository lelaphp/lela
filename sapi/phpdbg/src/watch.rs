//! Watchpoints (`Z/sapi/phpdbg/phpdbg_watch.c`), compared between steps
//! (#1308): php protects the pages a watched value lives on and, once a
//! write faulted, compares the value it backed up with what the page
//! holds at the next opline boundary; this host reads each watched value
//! at every boundary and compares it with the copy it kept, by the same
//! rules. A zval that changed its type, its scalar, or the string, array,
//! object or reference it points to is reported as php reports a
//! `WATCH_ON_ZVAL`; a string changed in place as a `WATCH_ON_STR`; an
//! array that grew or shrank as a `WATCH_ON_HASHTABLE`.

use php_sapi::tier::{FrameRef, Request};

use crate::cmd::Param;
use crate::info::flat;
use crate::out;
use crate::prompt::{Here, Ret};
use crate::read;
use crate::session::{self, Origin, Session};
use crate::shown::{self, Key, Shown};

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

/// Where php's registered watch element stands: on the zval, or on the
/// table of the array it holds (`PHPDBG_WATCH_ARRAY`), as a `watch
/// array`'s does and a recursive watch's over an array does, which
/// `phpdbg_create_recursive_watchpoint` recurses into at once and
/// registers the child it made on the table of (`$a[]`) in place of
/// the root on the variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Element {
    Zval,
    Table,
}

/// What frees the memory php watches for a watch as the request's
/// teardown destroys the global symbol table: php reports a watch
/// removed when `phpdbg_watch_efree` sees what it stood on freed, and
/// `zend_hash_graceful_reverse_destroy` destroys the variables from the
/// last back, each counted array a variable holds freed as it goes, its
/// elements first, then its data, then its table, before the table's
/// own data goes with every watch still standing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Freed {
    /// With the innermost counted array on the watch's path: `at` the
    /// place of each key down to it from the variable's own array, and
    /// `table` where php watches its table, its count, rather than an
    /// element in its data.
    Array { at: Vec<usize>, table: bool },
    /// With the symbol table's own data: no counted array holds what
    /// php watches.
    Table,
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
    /// Where php's element for it, the one announced and listed, stands.
    element: Element,
    /// What it held at the last boundary.
    old: Shown,
    /// Its variable's place in the frame's symbol table.
    place: usize,
    /// What frees it at the run's end, as of the last boundary.
    freed: Freed,
}

impl Watch {
    /// The name it was added and is listed under.
    fn listed(&self) -> Vec<u8> {
        match (self.kind, self.element) {
            (Kind::Recursive, Element::Table) => [self.name.as_slice(), b"[]"].concat(),
            _ => self.name.clone(),
        }
    }
}

/// The watchpoints of the session.
#[derive(Debug, Default, Clone)]
pub struct Watches {
    list: Vec<Watch>,
    next: u32,
    /// For each `ev` under way, the innermost last, what each watch read
    /// as it began: the place php watches, which the evaluation reaches
    /// through the copy of its variable `zend_attach_symbol_table` made,
    /// written back only as it ends, and through a separated copy of an
    /// array it writes into.
    frozen: Vec<Vec<(u32, Shown)>>,
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

/// What a watch reads now, and what would free it: none where its path
/// reaches nothing.
fn read(req: &mut Request, watch: &Watch) -> Option<(Shown, Freed)> {
    let root = shown::variable(req, watch.frame, &watch.variable, DEPTH)?;
    let value = walk(&root, &watch.path).cloned()?;
    Some((value, freed(&root, &watch.path, watch.element)))
}

/// [`Freed`] for what php watches down `path` from `root`: the element
/// in the data of each array the path passes, and the value at its end,
/// a counted array's table (`WATCH_ON_HASHTABLE`, or the
/// `WATCH_ON_REFCOUNTED` a zval holding one carries) or a counted
/// string, which its container frees as it destroys its elements. The
/// innermost counted one is freed first; an array that is not counted
/// holds none that is.
fn freed(root: &Shown, path: &[Vec<u8>], element: Element) -> Freed {
    let mut at = Vec::new();
    let mut found = Freed::Table;
    let mut node = deref(root);
    for key in path {
        let Shown::Array {
            counted, entries, ..
        } = node
        else {
            return found;
        };
        if *counted {
            found = Freed::Array {
                at: at.clone(),
                table: false,
            };
        }
        let Some(place) = entries.iter().position(|(k, _)| key_matches(k, key)) else {
            return found;
        };
        at.push(place);
        node = deref(&entries[place].1);
    }
    match node {
        Shown::Array { counted: true, .. } => Freed::Array { at, table: true },
        Shown::Str { counted: true, .. } if element == Element::Zval => {
            Freed::Array { at, table: false }
        }
        _ => found,
    }
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
        s.out.error(b"No active op array!");
        return;
    };
    let Some((variable, parts)) = parse(input) else {
        s.out.error(b"Malformed input");
        return;
    };
    let Some(root) = shown::variable(req, frame, &variable, DEPTH) else {
        s.out
            .error(format!("{} is undefined", lossy(input)).as_bytes());
        return;
    };
    let table = shown::variables(req, frame);
    let place = table
        .iter()
        .position(|row| row.name == variable)
        .unwrap_or(table.len());
    let named = [b"$".as_slice(), &variable].concat();
    for (path, name) in expand(&root, named, &parts) {
        let Some(old) = walk(&root, &path).cloned() else {
            s.out
                .error(format!("{} is undefined", lossy(&name)).as_bytes());
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
        let over_table = matches!(deref(&old), Shown::Array { .. } | Shown::Object { .. });
        let element = if kind == Kind::Array || (kind == Kind::Recursive && over_table) {
            Element::Table
        } else {
            Element::Zval
        };
        let freed = freed(&root, &path, element);
        let watch = Watch {
            id,
            kind,
            frame,
            variable: variable.clone(),
            path,
            name,
            element,
            old,
            place,
            freed,
        };
        // Only the recursive root announces itself as recursive.
        let root = kind == Kind::Recursive && element == Element::Zval;
        s.out.notice(
            format!(
                "Added{} watchpoint #{id} for {}",
                if root { " recursive" } else { "" },
                lossy(&watch.listed())
            )
            .as_bytes(),
        );
        s.watches.list.push(watch);
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
                s.out.notice(format!("Removed watchpoint {id}").as_bytes());
            }
            None => s
                .out
                .error(b"Nothing was deleted, no corresponding watchpoint found"),
        }
    }
    Ret::Success
}

/// `phpdbg_list_watchpoints`.
pub fn list(s: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    for watch in &s.watches.list {
        s.out.writeln(
            format!(
                "{} ({}, {})",
                lossy(&watch.listed()),
                match watch.element {
                    Element::Table => "array",
                    Element::Zval => "variable",
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
    read::exiting(req) || session::with(req, |s| s.stopped.is_some()).unwrap_or(true)
}

/// `phpdbg_print_watch_diff` for a zval: the labels to phpdbg's own
/// stream, the values through the request's output as
/// `zend_print_flat_zval_r` writes them with `zend_write`, so the first
/// byte sends the headers and a header callback runs, under the
/// debugger, before the rest is written; `false` where that ended the
/// request.
fn report_zval(req: &mut Request, name: &[u8], old: &Shown, new: &Shown) -> bool {
    session::say(req, |o| {
        o.notice(format!("Breaking on watchpoint {}", lossy(name)).as_bytes());
    });
    if old.counted() {
        session::say(req, |o| o.writeln(b"Old value inaccessible or destroyed"));
    } else {
        session::say(req, |o| o.out(b"Old value: "));
        out::interruptible(req, |req| req.write(&flat(old)));
        if ended(req) {
            return false;
        }
        session::say(req, |o| o.out(b"\n"));
    }
    let reference = matches!(new, Shown::Reference { .. });
    session::say(req, |o| {
        o.out(format!("New value{}: ", if reference { " (reference)" } else { "" }).as_bytes());
    });
    out::interruptible(req, |req| req.write(&flat(new)));
    if ended(req) {
        return false;
    }
    session::say(req, |o| o.out(b"\n"));
    true
}

/// `phpdbg_print_watch_diff` for a string changed in place, its bytes
/// written with `zend_write` as the zval's are.
fn report_str(req: &mut Request, name: &[u8], old: &[u8], new: &[u8]) -> bool {
    session::say(req, |o| {
        o.notice(format!("Breaking on watchpoint {}", lossy(name)).as_bytes());
    });
    session::say(req, |o| o.out(b"Old value: "));
    out::interruptible(req, |req| req.write(old));
    if ended(req) {
        return false;
    }
    session::say(req, |o| o.out(b"\n"));
    session::say(req, |o| o.out(b"New value: "));
    out::interruptible(req, |req| req.write(new));
    if ended(req) {
        return false;
    }
    session::say(req, |o| o.out(b"\n"));
    true
}

/// A key as php names an element by it.
fn key_text(key: &Key) -> Vec<u8> {
    match key {
        Key::Int(n) => n.to_string().into_bytes(),
        Key::Str(s) => s.clone(),
    }
}

/// What a recursive watch reports over the value at `name`, by
/// `phpdbg_watch.c`'s elements under its root: the zval at `name`
/// changed, as a simple watch reports it, after which its children are
/// made again over what it holds now; else, over the same array, the
/// table's element `name[]`, each key appended since announced from the
/// last back (`Element 2 has been added to watchpoint`) and a changed
/// count reported, then each element that stands in both, named
/// `name[key]`, by the same rules. `false` where a report ended the
/// request.
///
/// ```php
/// $a = [1, [2, intdiv(6, 2)]];
/// $a[1][1] = 9;
/// $a[] = 4;
/// echo "end\n";
/// ```
/// ```request phpdbg
/// b 3
/// r
/// w r $a
/// c
/// c
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003: $a[1][1] = 9;
///  00004: $a[] = 4;
///  00005: echo "end\n";
/// prompt> [Added watchpoint #0 for $a[]]
/// prompt> [Breaking on watchpoint $a[1][1]]
/// Old value: 3
/// New value: 9
/// >00004: $a[] = 4;
///  00005: echo "end\n";
/// prompt> [Element 2 has been added to watchpoint]
/// [Breaking on watchpoint $a[]]
/// 1 elements were added to the array
/// >00005: echo "end\n";
/// prompt> [$a has been removed, removing watchpoint recursively]
/// ```
fn recursive_report(req: &mut Request, name: &[u8], old: &Shown, new: &Shown) -> bool {
    if zval_differs(old, new) {
        session::with(req, |s| s.watch_hit = true);
        return report_zval(req, name, old, new);
    }
    if let Some((was, now)) = str_changed(old, new) {
        session::with(req, |s| s.watch_hit = true);
        return report_str(req, name, was, now);
    }
    let (
        Shown::Array {
            len: was,
            entries: before,
            ..
        },
        Shown::Array {
            len: now,
            entries: after,
            ..
        },
    ) = (deref(old), deref(new))
    else {
        return true;
    };
    let added = after
        .iter()
        .rev()
        .take_while(|(key, _)| !before.iter().any(|(held, _)| held == key));
    for (key, _) in added {
        session::with(req, |s| s.watch_hit = true);
        session::say(req, |o| {
            o.notice(
                [
                    &b"Element "[..],
                    &key_text(key),
                    b" has been added to watchpoint",
                ]
                .concat()
                .as_slice(),
            );
        });
    }
    if was != now {
        session::with(req, |s| s.watch_hit = true);
        session::say(req, |o| {
            o.notice(format!("Breaking on watchpoint {}[]", lossy(name)).as_bytes());
        });
        let message = if was > now {
            format!("{} elements were removed from the array", was - now)
        } else {
            format!("{} elements were added to the array", now - was)
        };
        session::say(req, |o| o.writeln(message.as_bytes()));
    }
    for (key, value) in after {
        let Some((_, held)) = before.iter().find(|(held, _)| held == key) else {
            continue;
        };
        let element = [name, b"[", &key_text(key), b"]"].concat();
        if !recursive_report(req, &element, held, value) {
            return false;
        }
    }
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
fn held(req: &mut Request, id: u32) -> Option<Shown> {
    session::with(req, |s| {
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
/// The session stands in the request's sink throughout, so a header callback a
/// report's first byte runs is checked by the debugger too, and finds
/// the same change, as php's does; the hit it takes is cleared before
/// this check reads it.
pub fn changed(req: &mut Request) -> bool {
    let Some((list, frozen)) = session::with(req, |s| {
        let frozen = (s.origin == Origin::Eval)
            .then(|| s.watches.frozen.last().cloned())
            .flatten();
        (s.watches.list.clone(), frozen)
    }) else {
        return false;
    };
    for watch in list {
        // Inside what `ev` evaluates, a watch reads what it read as the
        // evaluation began.
        let read = match &frozen {
            Some(frozen) => frozen
                .iter()
                .find(|(id, _)| *id == watch.id)
                .map(|(_, held)| (held.clone(), watch.freed.clone())),
            _ => read(req, &watch),
        };
        let Some((new, freed)) = read else {
            session::say(req, |o| {
                o.notice(
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
            });
            session::with(req, |s| s.watches.list.retain(|w| w.id != watch.id));
            continue;
        };
        let Some(old) = held(req, watch.id) else {
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
                    session::with(req, |s| s.watch_hit = true);
                    session::say(req, |o| {
                        o.notice(
                            format!("Breaking on watchpoint {}", lossy(&watch.name)).as_bytes(),
                        );
                    });
                    let message = if old > new {
                        format!("{} elements were removed from the array", old - new)
                    } else {
                        format!("{} elements were added to the array", new - old)
                    };
                    session::say(req, |o| o.writeln(message.as_bytes()));
                }
                true
            }
            Kind::Recursive => recursive_report(req, &watch.name, &old, &new),
            Kind::Simple => {
                if zval_differs(&old, &new) {
                    session::with(req, |s| s.watch_hit = true);
                    report_zval(req, &watch.name, &old, &new)
                } else if let Some((was, now)) = str_changed(&old, &new) {
                    session::with(req, |s| s.watch_hit = true);
                    report_str(req, &watch.name, was, now)
                } else {
                    true
                }
            }
        };
        if !reported {
            return false;
        }
        session::with(req, |s| {
            if let Some(kept) = s.watches.list.iter_mut().find(|w| w.id == watch.id) {
                kept.old = new;
                kept.freed = freed;
            }
        });
    }
    session::with(req, |s| std::mem::take(&mut s.watch_hit)).unwrap_or(false)
}

/// What each watch reads as an `ev` begins, kept for the length of the
/// evaluation.
pub fn freeze(s: &mut Session, req: &mut Request) {
    let held = s
        .watches
        .list
        .iter()
        .filter_map(|watch| read(req, watch).map(|(value, _)| (watch.id, value)))
        .collect();
    s.watches.frozen.push(held);
}

/// The evaluation over: what [`freeze`] kept for it let go.
pub fn thaw(s: &mut Session) {
    s.watches.frozen.pop();
}

/// The watchpoints left when a run ends, each reported removed as the
/// request's teardown frees what php watches for it ([`Freed`]): those
/// on counted values as the variables are destroyed from the last back,
/// each array's after those of what it holds, in the order of its keys,
/// its data before its table; then those on the symbol table's own data,
/// in the order of its watched names, each name's in the order they were
/// added. The names are php's `hti->watches` over the symbol table, one
/// per variable, which each removal before drops its variable's name
/// from, and which `phpdbg_unwatch_parent_ht` destroys whole where it
/// holds one name as a removal comes, whichever: no watch on the table's
/// own data is reported after that.
///
/// ```php
/// $a = [1, intdiv(4, 2)];
/// $x = 1;
/// $x = 2;
/// echo "end\n";
/// ```
/// ```request phpdbg
/// b 4
/// r
/// w a $a
/// w $x
/// c
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:4]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:4, hits: 1]
/// >00004: $x = 2;
///  00005: echo "end\n";
/// prompt> [Added watchpoint #0 for $a[]]
/// prompt> [Added watchpoint #1 for $x]
/// prompt> [Breaking on watchpoint $x]
/// Old value: 1
/// New value: 2
/// >00005: echo "end\n";
/// prompt> end
/// [$a[] has been removed, removing watchpoint]
/// [$x has been removed, removing watchpoint]
/// [Script ended normally]
/// prompt>
/// ```
pub fn removed_at_end(s: &mut Session) {
    let list = std::mem::take(&mut s.watches.list);
    let (mut arrays, table): (Vec<&Watch>, Vec<&Watch>) = list
        .iter()
        .partition(|watch| matches!(watch.freed, Freed::Array { .. }));
    // A stable sort, so watches on one array keep the order they were
    // added in.
    arrays.sort_by(|a, b| match (&a.freed, &b.freed) {
        (
            Freed::Array {
                at: at_a,
                table: ta,
            },
            Freed::Array {
                at: at_b,
                table: tb,
            },
        ) => b
            .place
            .cmp(&a.place)
            .then_with(|| destroyed_first(at_a, at_b))
            .then(ta.cmp(tb)),
        _ => std::cmp::Ordering::Equal,
    });
    let mut names: Vec<&[u8]> = Vec::new();
    for watch in &list {
        if !names.contains(&watch.variable.as_slice()) {
            names.push(&watch.variable);
        }
    }
    for watch in &arrays {
        if names.len() == 1 {
            names.clear();
        } else {
            names.retain(|name| *name != watch.variable.as_slice());
        }
    }
    let table = names
        .iter()
        .flat_map(|name| table.iter().filter(move |watch| watch.variable == *name));
    for watch in arrays.iter().chain(table) {
        s.out.notice(
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

/// The order `zend_array_destroy` frees two arrays of one variable in,
/// each named by the places of the keys down to it: an array's elements
/// are destroyed in the order of its keys, so the arrays under an
/// element go before those under a later one, and before the array that
/// holds them.
fn destroyed_first(a: &[usize], b: &[usize]) -> std::cmp::Ordering {
    match a.iter().zip(b).find(|(x, y)| x != y) {
        Some((x, y)) => x.cmp(y),
        None => b.len().cmp(&a.len()),
    }
}
