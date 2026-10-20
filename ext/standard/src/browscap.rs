//! `Z/ext/standard/browscap.c`: `get_browser` over the file the
//! `browscap` ini entry names, read with Zend's ini grammar under
//! `ZEND_INI_SCANNER_RAW`
//! (`crate::ini`) into php's table of patterns, each with its `Parent`
//! and its lowercased keys. php reads the file at `MINIT`, as the
//! entry is `PHP_INI_SYSTEM`; here it is read on the request's first
//! `get_browser` and kept on the request state, which answers the same.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ops::ControlFlow;

use php_ext::abi::{OrFalse, Union};
use php_ext::attr::php_function;
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value};

use crate::State;
use php_ext::syntax::{IniEvent as Event, IniScanner, IniValue as Scalar};

/// `BROWSCAP_NUM_CONTAINS`: how many fixed runs after the prefix a
/// pattern keeps for the quick refusal.
const NUM_CONTAINS: usize = 5;

/// `DEFAULT_SECTION_NAME`, the entry a user agent nothing matches gets.
const DEFAULT_SECTION_NAME: &[u8] = b"Default Browser Capability Settings";

/// `browscap_entry`: the section's pattern as written, its `Parent`,
/// its keys and values in order, and what `browser_reg_compare` reads to
/// refuse quickly.
struct BrowscapEntry {
    pattern: Vec<u8>,
    parent: Option<Vec<u8>>,
    kv: Vec<(Vec<u8>, Vec<u8>)>,
    prefix_len: usize,
    contains: [(usize, usize); NUM_CONTAINS],
}

/// `browser_data`: php's `htab`, sections by their pattern in the order
/// they were first seen, a repeated one replacing the first in place,
/// loaded from the file the `browscap` entry names. The entry is
/// `main.c`'s, `PHP_INI_SYSTEM` with no value, and so Core's, not this
/// extension's:
///
/// ```php
/// var_dump(ini_get('browscap'), ini_set('browscap', 'x'), array_key_exists('browscap', ini_get_all('standard')), array_key_exists('unserialize_callback_func', ini_get_all('standard')));
/// ```
/// ```expect
/// string(0) ""
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[derive(Default)]
pub(crate) struct Browscap {
    loaded: bool,
    entries: Vec<BrowscapEntry>,
}

impl Browscap {
    fn find(&self, pattern: &[u8]) -> Option<&BrowscapEntry> {
        self.entries.iter().find(|entry| entry.pattern == pattern)
    }
}

fn is_placeholder(c: u8) -> bool {
    c == b'?' || c == b'*'
}

/// `browscap_compute_prefix_len` and `browscap_compute_contains`
/// (`Z/ext/standard/browscap.c:91-127`): the prefix free of wildcards,
/// capped at 255, then up to five runs of two or more fixed bytes, each
/// capped at 255.
fn compute(pattern: &[u8]) -> (usize, [(usize, usize); NUM_CONTAINS]) {
    let len = pattern.len();
    let prefix = pattern
        .iter()
        .position(|&c| is_placeholder(c))
        .unwrap_or(len);
    let mut contains = [(0, 0); NUM_CONTAINS];
    let mut pos = prefix;
    for slot in &mut contains {
        let mut i = pos;
        while i < len {
            if !is_placeholder(pattern[i]) && i + 1 < len && !is_placeholder(pattern[i + 1]) {
                break;
            }
            i += 1;
        }
        let start = i;
        while i < len && !is_placeholder(pattern[i]) {
            i += 1;
        }
        *slot = (start, (i - start).min(255));
        pos = i;
    }
    (prefix.min(255), contains)
}

/// `php_browscap_parser_cb` (`Z/ext/standard/browscap.c:302`) over the
/// raw parse: a section begins an entry; an entry's `Parent` names its
/// parent, and every other key, lowercased, holds its value, `on`, `yes`
/// and `true` as `1` and `no`, `off`, `none` and `false` as the empty
/// string; a key before any section, or with no value, is dropped.
fn load(events: Vec<Event>) -> Vec<BrowscapEntry> {
    let text = |value: &Scalar| match value {
        Scalar::Str(bytes) => bytes.clone(),
        _ => Vec::new(),
    };
    let mut entries: Vec<BrowscapEntry> = Vec::new();
    let mut current: Option<usize> = None;
    for event in events {
        match event {
            Event::Section(name) => {
                let pattern = text(&name);
                if pattern.len() > usize::from(u16::MAX) {
                    current = None;
                    continue;
                }
                let (prefix_len, contains) = compute(&pattern);
                let entry = BrowscapEntry {
                    pattern: pattern.clone(),
                    parent: None,
                    kv: Vec::new(),
                    prefix_len,
                    contains,
                };
                if let Some(at) = entries.iter().position(|e| e.pattern == pattern) {
                    entries[at] = entry;
                    current = Some(at);
                } else {
                    entries.push(entry);
                    current = Some(entries.len() - 1);
                }
            }
            Event::Entry(key, Some(value)) => {
                let Some(entry) = current.and_then(|at| entries.get_mut(at)) else {
                    continue;
                };
                let value = text(&value);
                let lower = value.to_ascii_lowercase();
                let value = match lower.as_slice() {
                    b"on" | b"yes" | b"true" => b"1".to_vec(),
                    b"no" | b"off" | b"none" | b"false" => Vec::new(),
                    _ => value,
                };
                let key = text(&key);
                if key.eq_ignore_ascii_case(b"parent") {
                    entry.parent = Some(value);
                } else {
                    entry.kv.push((key.to_ascii_lowercase(), value));
                }
            }
            Event::Entry(_, None) | Event::PopEntry(..) => {}
        }
    }
    entries
}

/// `browscap_match_string_wildcard` (`Z/ext/standard/browscap.c:547`):
/// `*` any stretch of bytes, `?` any one byte, with php's backtracking.
fn match_wildcard(s: &[u8], pattern: &[u8]) -> bool {
    let (mut p, mut i) = (0, 0);
    let mut restore: Option<(usize, usize)> = None;
    while i < s.len() {
        let pc = pattern.get(p).copied().unwrap_or(0);
        if pc == b'*' {
            while pattern.get(p) == Some(&b'*') {
                p += 1;
            }
            if p == pattern.len() {
                return true;
            }
            if pattern[p] != b'?' {
                while i < s.len() && s[i] != pattern[p] {
                    i += 1;
                }
            }
            restore = Some((p, i));
            continue;
        }
        if pc == s[i] || pc == b'?' {
            p += 1;
            i += 1;
            if p != pattern.len() {
                continue;
            }
            if i == s.len() {
                return true;
            }
        }
        let Some((rp, ri)) = restore else {
            return false;
        };
        p = rp;
        restore = Some((rp, ri + 1));
        i = ri + 1;
    }
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

/// `browser_reg_compare` (`Z/ext/standard/browscap.c:622`): whether the
/// lowercased agent holds the entry's fixed runs in order and matches
/// its lowercased pattern, and how many fixed bytes the pattern has,
/// the measure the best match is chosen by.
fn compare(entry: &BrowscapEntry, agent: &[u8]) -> Option<Match> {
    let pattern = entry.pattern.to_ascii_lowercase();
    let mut cur = entry.prefix_len;
    for &(start, len) in &entry.contains {
        if len == 0 {
            continue;
        }
        let needle = pattern.get(start..start + len)?;
        let found = agent.get(cur..)?.windows(len).position(|w| w == needle)?;
        cur += found + len;
    }
    if agent == pattern.as_slice() {
        return Some(Match::Exact);
    }
    let from = entry.prefix_len;
    if !match_wildcard(agent.get(from..)?, pattern.get(from..)?) {
        return None;
    }
    let fixed = entry.prefix_len
        + entry.pattern[entry.prefix_len..]
            .iter()
            .filter(|&&c| !is_placeholder(c))
            .count();
    Some(Match::Wildcard(fixed))
}

/// What `browser_reg_compare` found: the agent itself, which ends the
/// search, or a match by wildcards and its count of fixed bytes.
enum Match {
    Exact,
    Wildcard(usize),
}

/// `browscap_entry_add_kv_to_existing_array`: the entry's keys and
/// values after the rows, each key the rows have not got yet, as
/// `zend_hash_add` adds it.
fn add(rows: &mut Vec<(Vec<u8>, Vec<u8>)>, entry: &BrowscapEntry) {
    for (key, value) in &entry.kv {
        if !rows.iter().any(|(k, _)| k == key) {
            rows.push((key.clone(), value.clone()));
        }
    }
}

/// `browscap_convert_pattern` (`Z/ext/standard/browscap.c:148`): the
/// pattern as the regular expression `browser_name_regex` shows.
fn convert_pattern(pattern: &[u8]) -> Vec<u8> {
    let mut out = b"~^".to_vec();
    for &c in pattern {
        match c {
            b'?' => out.push(b'.'),
            b'*' => out.extend_from_slice(b".*"),
            b'.' | b'\\' | b'(' | b')' | b'~' | b'+' => {
                out.push(b'\\');
                out.push(c);
            }
            _ => out.push(c.to_ascii_lowercase()),
        }
    }
    out.extend_from_slice(b"$~");
    out
}

/// `browscap_read_file` (`Z/ext/standard/browscap.c:402`) on first use:
/// the file the entry names parsed raw, or nothing where it names none
/// or one that cannot be read.
fn loaded(req: &mut Request) -> Result<(), Thrown> {
    if req.state::<State>().browscap.loaded {
        return Ok(());
    }
    req.state::<State>().browscap.loaded = true;
    let Some(file) = req.ini_get(b"browscap").map(<[u8]>::to_vec) else {
        return Ok(());
    };
    if file.is_empty() {
        return Ok(());
    }
    let path = req.absolutize(&file);
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    let mut events: Vec<Event> = Vec::new();
    let parsed = php_ext::syntax::parse_ini(req, &bytes, IniScanner::Raw, |_, event| {
        // The raw scanner's values are strings, kept as bytes.
        let text = |value: Value| match value.as_str_bytes() {
            Some(bytes) => Scalar::Str(bytes.to_vec()),
            None => Scalar::Null,
        };
        events.push(match event {
            Event::Section(name) => Event::Section(text(name)),
            Event::Entry(label, value) => Event::Entry(text(label), value.map(text)),
            Event::PopEntry(label, value, offset) => {
                Event::PopEntry(text(label), text(value), text(offset))
            }
        });
        Ok(())
    })?;
    if parsed.is_err() {
        events.clear();
    }
    req.state::<State>().browscap.entries = load(events);
    Ok(())
}

/// The entry `get_browser` answers with for the lowercased agent: the
/// section of that name, else the best wildcard match, else the default
/// section (`Z/ext/standard/browscap.c:744-785`).
fn lookup_entry(data: &Browscap, lookup: &[u8]) -> Option<usize> {
    let mut found = data.entries.iter().position(|e| e.pattern == lookup);
    if found.is_none() {
        let mut best = 0;
        for (at, entry) in data.entries.iter().enumerate() {
            let minimum = entry.prefix_len + entry.contains.iter().map(|c| c.1).sum::<usize>();
            if lookup.len() < minimum {
                continue;
            }
            let prefix = entry.pattern.get(..entry.prefix_len).unwrap_or_default();
            if !lookup
                .get(..entry.prefix_len)
                .is_some_and(|l| l.eq_ignore_ascii_case(prefix))
            {
                continue;
            }
            match compare(entry, lookup) {
                Some(Match::Exact) => {
                    found = Some(at);
                    break;
                }
                Some(Match::Wildcard(fixed)) if found.is_none() || best < fixed => {
                    found = Some(at);
                    best = fixed;
                }
                _ => {}
            }
        }
    }
    found.or_else(|| {
        data.entries
            .iter()
            .position(|e| e.pattern == DEFAULT_SECTION_NAME)
    })
}

/// `get_browser(?string $user_agent = null, bool $return_array = false): object|array|false`,
/// `PHP_FUNCTION(get_browser)` (`Z/ext/standard/browscap.c:699`): the
/// agent, `$_SERVER['HTTP_USER_AGENT']` where none is given, lowercased
/// and looked up as a section first, else matched against every pattern,
/// the one with the most fixed bytes winning, else the default section;
/// its `browser_name_regex`, `browser_name_pattern`, `parent` and keys,
/// then each parent's keys it has not got, as an object or an array.
///
/// ```php
/// var_dump(get_browser('Mozilla/5.0'));
/// ```
/// ```expect
/// Warning: get_browser(): browscap ini directive not set in %s on line 2
/// bool(false)
/// ```
///
/// ```php
/// echo new ReflectionFunction('get_browser');
/// ```
/// ```expect
/// Function [ <internal:standard> function get_browser ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <optional> ?string $user_agent = null ]
///     Parameter #1 [ <optional> bool $return_array = false ]
///   }
///   - Return [ object|array|false ]
/// }
/// ```
#[php_function]
fn get_browser(
    req: &mut Request,
    #[php(default = null)] user_agent: Option<PhpString>,
    #[php(default = false)] return_array: bool,
) -> Result<Union<Owner<PhpObject>, OrFalse<Owner<PhpArray>>>, Thrown> {
    let agent = user_agent;
    loaded(req)?;
    if req.state::<State>().browscap.entries.is_empty() {
        docref(req, Level::Warning, "browscap ini directive not set");
        return Ok(Union::B(OrFalse::False));
    }
    let agent = if let Some(agent) = agent {
        agent.as_bytes().to_vec()
    } else {
        {
            let found = req
                .variables_mut()
                .superglobal(b"_SERVER")
                .and_then(|server| server.find(b"HTTP_USER_AGENT"))
                .and_then(|value| value.deref().as_str_bytes().map(<[u8]>::to_vec));
            let Some(found) = found else {
                docref(
                    req,
                    Level::Warning,
                    "HTTP_USER_AGENT variable is not set, cannot determine user agent name",
                );
                return Ok(Union::B(OrFalse::False));
            };
            found
        }
    };
    let lookup = agent.to_ascii_lowercase();
    let data = &req.state::<State>().browscap;
    let found = lookup_entry(data, &lookup);
    let Some(found) = found else {
        return Ok(Union::B(OrFalse::False));
    };
    let mut rows: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let entry = &data.entries[found];
    rows.push((
        b"browser_name_regex".to_vec(),
        convert_pattern(&entry.pattern),
    ));
    rows.push((b"browser_name_pattern".to_vec(), entry.pattern.clone()));
    if let Some(parent) = &entry.parent {
        rows.push((b"parent".to_vec(), parent.clone()));
    }
    add(&mut rows, entry);
    let mut parent = entry.parent.as_deref();
    while let Some(name) = parent {
        let Some(next) = data.find(name) else {
            break;
        };
        add(&mut rows, next);
        parent = next.parent.as_deref();
    }
    let mut array = PhpArray::new(req)?;
    for (key, value) in rows {
        let value = PhpString::new(req, &value)?.into_value();
        array.set_str(req, &key, value)?;
    }
    if return_array {
        return Ok(Union::B(OrFalse::Value(array)));
    }
    let Some(class) = lookup_class(req, b"stdClass")? else {
        return Ok(Union::B(OrFalse::Value(array)));
    };
    let object = new_object(req, class)?;
    // `object_and_properties_init` over the browser's row
    // (`Z/ext/standard/browscap.c:793`), which runs no code of the program;
    // each row is lent while it is written.
    walk::foreach(req, *array, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        if let Key::Str(name) = key {
            let value = Owner::take(req, value);
            let name = Owner::take(req, name);
            object.set_prop(req, name, value)?;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    array.give_up(req);
    Ok(Union::A(object))
}
