//! Declarations made once for every request: the headers `ffi.preload`
//! names, loaded at startup (`MINIT`) into the engine state, and the ones
//! `FFI::load` loads while `opcache.preload`'s file is served, kept in
//! the engine's cache (ADR-0005). Both are immutable once made; a
//! request's `FFI::scope` lays a scope's types into its own arena the
//! first time it names the scope. This is `zend_ffi_load` with its
//! `preload` argument, `zend_ffi_preload` and `zend_ffi_parse_directives`.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use php_ext::abi::{Cache, EngineState, Refusal, Start};
use php_ext::raising::{Level, warning};
use php_ext::request::Request;

use crate::ffi::{self, Library};
use crate::parse::{Parser, Scope, SymbolKind, TagKind};
use crate::throwables::Throwables;
use crate::types::{TypeId, Types, attr};
use crate::{PRELOAD, State, types};

/// What a header loaded: its declarations, the library `FFI_LIB` named,
/// and the scope `FFI_SCOPE` named.
pub(crate) struct Loaded {
    pub(crate) scope: Scope,
    pub(crate) lib: Option<Library>,
    pub(crate) scope_name: Option<Vec<u8>>,
}

/// Why a header did not load, in the order `zend_ffi_load` finds it.
pub(crate) enum Fail {
    Missing,
    NotFile,
    Unreadable,
    BadDefine(&'static str),
    Twice(&'static str),
    Parse(String),
    Lib(Vec<u8>, String),
    Symbol(&'static str, Vec<u8>),
    /// What `zend_ffi_same_symbols` or `zend_ffi_same_tags` refused
    /// against the scope a preloaded header adds to, as php names it.
    Redefined(String),
}

/// The file a header's path names while a request runs: a relative path
/// against the request's working directory, where php's `stat` and
/// `open` find it against its process's, which `chdir()` moved there
/// (#2618). At startup no request stands, and the path is the process's
/// to resolve.
pub(crate) fn header_file(req: &Request, path: &[u8]) -> PathBuf {
    req.environment().cwd().join(OsStr::from_bytes(path))
}

/// The library a declaration set names, as `dlopen` is to open it while
/// a request runs: a relative path, one that names a directory, against
/// the request's working directory `cwd`, where php's `dlopen` resolves
/// it against its process's, which `chdir()` moved there; a bare name,
/// which `dlopen` searches for, and an absolute path as written. At
/// startup no request stands and `cwd` is none.
pub(crate) fn library_path<'a>(cwd: Option<&Path>, name: &'a [u8]) -> Cow<'a, [u8]> {
    match cwd {
        Some(cwd) if name.contains(&b'/') && !name.starts_with(b"/") => Cow::Owned(
            cwd.join(OsStr::from_bytes(name))
                .into_os_string()
                .into_vec(),
        ),
        _ => Cow::Borrowed(name),
    }
}

/// `zend_ffi_load` up to its scope: the file read, its directives, its
/// declarations parsed into `types`, the library opened and every
/// variable and function resolved; at startup, each held against the
/// scope of `scopes` it adds to. `cwd` is the request's working
/// directory, against which a relative `FFI_LIB` is opened
/// ([`library_path`]), none at startup.
pub(crate) fn load_header(
    file: &Path,
    cwd: Option<&Path>,
    types: &mut Types,
    default_attr: u32,
    scopes: Option<&[(Vec<u8>, Scope)]>,
) -> Result<Loaded, Fail> {
    let meta = std::fs::metadata(file).map_err(|_| Fail::Missing)?;
    if !meta.is_file() {
        return Err(Fail::NotFile);
    }
    let code = std::fs::read(file).map_err(|_| Fail::Unreadable)?;
    let (start, scope_name, lib) = directives(&code)?;
    let mut scope = Scope::default();
    let body = code.get(start..).unwrap_or_default();
    Parser::new(body, types, &mut scope, default_attr)
        .parse_decl()
        .map_err(|bail| Fail::Parse(bail.0))?;
    let lib = match lib {
        Some(name) => {
            Some(Library::open(&library_path(cwd, &name)).map_err(|error| Fail::Lib(name, error))?)
        }
        None => None,
    };
    let name = scope_name.as_deref().unwrap_or(b"C");
    let old = scopes.and_then(|scopes| scopes.iter().find(|(have, _)| have == name));
    let old = old.map(|(_, old)| (old, &*types));
    resolve_symbols(&mut scope, lib.as_ref(), old)?;
    if let Some((old, types)) = old {
        same_tags(&scope, old, types)?;
    }
    Ok(Loaded {
        scope,
        lib,
        scope_name,
    })
}

/// `DL_FETCH_SYMBOL` over each variable and function of `scope`, in
/// `lib` or in the process, the first missing named by its kind; and
/// each symbol, in its turn, held by `zend_ffi_same_symbols` to the one
/// of the same name in `old`, the scope a preloaded header adds to.
pub(crate) fn resolve_symbols(
    scope: &mut Scope,
    lib: Option<&Library>,
    old: Option<(&Scope, &Types)>,
) -> Result<(), Fail> {
    let Some(symbols) = scope.symbols.as_mut() else {
        return Ok(());
    };
    for (name, sym) in symbols.iter_mut() {
        let what = match sym.kind {
            SymbolKind::Var => Some("variable"),
            SymbolKind::Func => Some("function"),
            _ => None,
        };
        if let Some(what) = what {
            let addr = match lib {
                Some(lib) => lib.symbol(name),
                None => ffi::default_symbol(name),
            };
            let Some(addr) = addr else {
                return Err(Fail::Symbol(what, name.to_vec()));
            };
            sym.addr = addr;
        }
        let Some((old, types)) = old else {
            continue;
        };
        if let Some(was) = old.symbols.as_ref().and_then(|symbols| symbols.get(name)) {
            let same = was.kind == sym.kind
                && was.is_const == sym.is_const
                && (sym.kind != SymbolKind::Const || was.value == sym.value)
                && types.same_types(was.ty, sym.ty);
            if !same {
                return Err(Fail::Redefined(String::from_utf8_lossy(name).into_owned()));
            }
        }
    }
    Ok(())
}

/// `zend_ffi_same_tags` over each tag of `scope` that `old` has too.
fn same_tags(scope: &Scope, old: &Scope, types: &Types) -> Result<(), Fail> {
    let (Some(tags), Some(was)) = (scope.tags.as_ref(), old.tags.as_ref()) else {
        return Ok(());
    };
    for (name, tag) in tags.iter() {
        let Some(was) = was.get(name) else {
            continue;
        };
        if was.kind != tag.kind || !types.same_types(was.ty, tag.ty) {
            let kind = match tag.kind {
                TagKind::Enum => "enum",
                TagKind::Struct => "struct",
                TagKind::Union => "union",
            };
            let name = String::from_utf8_lossy(name);
            return Err(Fail::Redefined(format!("{kind} {name}")));
        }
    }
    Ok(())
}

/// Why `zend_ffi_parse_directives` refused a file.
fn bad(what: &'static str) -> Fail {
    Fail::BadDefine(what)
}

/// `zend_ffi_skip_ws_and_comments`.
fn skip_ws_and_comments(code: &[u8], mut at: usize, newlines: bool) -> usize {
    let b = |i: usize| code.get(i).copied().unwrap_or(0);
    loop {
        match b(at) {
            b' ' | b'\t' => at += 1,
            b'\r' | b'\n' | 0x0c | 0x0b if newlines => at += 1,
            b'/' if newlines && b(at + 1) == b'/' => {
                at += 2;
                while b(at) != 0 && b(at) != b'\r' && b(at) != b'\n' {
                    at += 1;
                }
            }
            b'/' if b(at + 1) == b'*' => {
                at += 2;
                while b(at) != 0 && !(b(at) == b'*' && b(at + 1) == b'/') {
                    at += 1;
                }
                if b(at) == b'*' {
                    at += 1;
                    if b(at) == b'/' {
                        at += 1;
                    }
                }
            }
            _ => return at,
        }
    }
}

/// The scope name, the library and where the declarations begin.
type Directives = (usize, Option<Vec<u8>>, Option<Vec<u8>>);

/// `zend_ffi_parse_directives`: the `#define FFI_SCOPE "..."` and
/// `#define FFI_LIB "..."` lines a header opens with.
fn directives(code: &[u8]) -> Result<Directives, Fail> {
    let b = |i: usize| code.get(i).copied().unwrap_or(0);
    let mut at = skip_ws_and_comments(code, 0, true);
    let (mut scope, mut lib) = (None, None);
    while b(at) == b'#' {
        if !code
            .get(at..)
            .is_some_and(|rest| rest.starts_with(b"#define"))
        {
            break;
        }
        let mut p = skip_ws_and_comments(code, at + 7, false);
        let rest = code.get(p..).unwrap_or_default();
        let (target, name) = if rest.starts_with(b"FFI_SCOPE") {
            p = skip_ws_and_comments(code, p + 9, false);
            (&mut scope, "FFI_SCOPE")
        } else if rest.starts_with(b"FFI_LIB") {
            p = skip_ws_and_comments(code, p + 7, false);
            (&mut lib, "FFI_LIB")
        } else {
            while b(p) != 0 && b(p) != b'\n' && b(p) != b'\r' {
                p += 1;
            }
            at = skip_ws_and_comments(code, p, true);
            continue;
        };
        if b(p) != b'"' {
            return Err(bad(name));
        }
        p += 1;
        if target.is_some() {
            return Err(Fail::Twice(name));
        }
        let open = p;
        loop {
            if b(p) == b'"' {
                *target = Some(code.get(open..p).unwrap_or_default().to_vec());
                p += 1;
                break;
            } else if b(p) <= b' ' {
                return Err(bad(name));
            }
            p += 1;
        }
        at = skip_ws_and_comments(code, p, true);
    }
    Ok((at, scope, lib))
}

/// `zend_ffi_load(filename, 1)`'s warnings, as php raises them.
fn preload_warning(path: &str, fail: &Fail) -> String {
    match fail {
        Fail::Missing => format!("FFI: failed pre-loading '{path}', file doesn't exist"),
        Fail::NotFile => format!("FFI: failed pre-loading '{path}', not a regular file"),
        Fail::Unreadable => format!("FFI: Failed pre-loading '{path}', cannot read_file"),
        Fail::BadDefine(what) => format!("FFI: failed pre-loading '{path}', bad {what} define"),
        Fail::Twice(what) => format!("FFI: failed pre-loading '{path}', {what} defined twice"),
        Fail::Parse(_) => format!("FFI: failed pre-loading '{path}'"),
        Fail::Lib(lib, _) => format!("FFI: Failed pre-loading '{}'", String::from_utf8_lossy(lib)),
        Fail::Symbol("variable", name) => format!(
            "FFI: failed pre-loading '{path}', cannot resolve C variable '{}'",
            String::from_utf8_lossy(name)
        ),
        Fail::Symbol(_, name) => format!(
            "failed pre-loading '{path}', cannot resolve C function '{}'",
            String::from_utf8_lossy(name)
        ),
        Fail::Redefined(what) => {
            format!("FFI: failed pre-loading '{path}', redefinition of '{what}'")
        }
    }
}

/// `zend_ffi_load(filename, 1)`'s warnings for a header that failed:
/// `zend_ffi_parser_error`'s first where the parser refused it.
fn preload_warnings(path: &[u8], fail: &Fail) -> Vec<String> {
    let shown = String::from_utf8_lossy(path);
    let mut warnings = Vec::new();
    if let Fail::Parse(message) = fail {
        warnings.push(format!("FFI Parser: {message}"));
    }
    warnings.push(preload_warning(&shown, fail));
    warnings
}

/// What preloading made, for every request: the scopes `ffi.preload`
/// loaded over one arena, and the cache the opcache preload's
/// `FFI::load` registers its scopes in.
#[derive(EngineState)]
pub(crate) struct Preloads {
    types: Arc<Types>,
    startup: Vec<(Vec<u8>, Arc<Scope>)>,
    registered: Cache<Vec<u8>, Option<(Types, Scope)>>,
    /// The classes the natives raise, resolved at this same start.
    pub(crate) throwables: Throwables,
}

/// `zend_ffi_preload(FFI_G(preload))` at startup (`MINIT`): each path of
/// the list, `:`-separated, a glob where it has a `*`, `?` or `[`, loaded
/// into its scope, `C` where the header names none. The first that fails
/// fails the start, `PHP_MINIT_FUNCTION(ffi)` returning `FAILURE`: its
/// warnings are raised, then the engine's `Unable to start FFI module`,
/// and the host ends the process with `php_error_cb`'s `exit(-2)`
/// (`Z/main/main.c:1505-1507`).
///
/// The test `a_failed_preload_refuses_startup` holds a missing file and
/// a redeclaration to the oracle's bytes.
pub(crate) fn engine_state(start: &Start) -> Result<Preloads, Refusal> {
    let throwables = Throwables::start(start)?;
    let mut types = Types::default();
    let mut scopes: Vec<(Vec<u8>, Scope)> = Vec::new();
    let list = start
        .ini()
        .entries
        .iter()
        .find(|(name, _)| name.as_slice() == PRELOAD.as_bytes())
        .and_then(|(_, value)| value.clone())
        .unwrap_or_default();
    for part in list.split(|b| *b == b':').filter(|part| !part.is_empty()) {
        let paths = if part.iter().any(|b| matches!(b, b'*' | b'?' | b'[')) {
            glob(part)
        } else {
            vec![part.to_vec()]
        };
        for path in paths {
            let preload = attr::STORED | attr::PERSISTENT;
            let file = Path::new(OsStr::from_bytes(&path));
            let loaded = match load_header(file, None, &mut types, preload, Some(&scopes)) {
                Ok(loaded) => loaded,
                Err(fail) => {
                    let warnings = preload_warnings(&path, &fail).into_iter();
                    return Err(warnings.fold(Refusal::new(), |refusal, warning| {
                        refusal.raise(Level::Warning, warning)
                    }));
                }
            };
            let name = loaded.scope_name.unwrap_or_else(|| b"C".to_vec());
            if let Some(lib) = loaded.lib {
                #[expect(
                    clippy::mem_forget,
                    reason = "a preloaded library stays loaded for the process's life, its symbols named by the scope"
                )]
                std::mem::forget(lib);
            }
            merge(&mut scopes, name, loaded.scope);
        }
    }
    Ok(Preloads {
        types: Arc::new(types),
        startup: scopes
            .into_iter()
            .map(|(name, scope)| (name, Arc::new(scope)))
            .collect(),
        registered: Cache::new(1024),
        throwables,
    })
}

/// A scope merged into the one of the same name, each name kept where
/// it stood first, as php adds what the old scope lacks.
fn merge(scopes: &mut Vec<(Vec<u8>, Scope)>, name: Vec<u8>, scope: Scope) {
    let Some((_, old)) = scopes.iter_mut().find(|(have, _)| *have == name) else {
        scopes.push((name, scope));
        return;
    };
    if let Some(symbols) = scope.symbols {
        let into = old.symbols.get_or_insert_with(Default::default);
        for (key, sym) in symbols.iter() {
            into.add(key, sym.clone());
        }
    }
    if let Some(tags) = scope.tags {
        let into = old.tags.get_or_insert_with(Default::default);
        for (key, tag) in tags.iter() {
            into.add(key, *tag);
        }
    }
}

/// `php_glob(pattern, 0)` over the pattern's last component: the
/// directory's entries it matches, sorted as `glob` sorts them.
fn glob(pattern: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(pattern).into_owned();
    let (dir, name) = match text.rfind('/') {
        Some(at) => (
            text.get(..at).unwrap_or("/").to_string(),
            text.get(at + 1..).unwrap_or("").to_string(),
        ),
        None => (String::from("."), text.clone()),
    };
    let Ok(entries) = std::fs::read_dir(if dir.is_empty() { "/" } else { &dir }) else {
        return Vec::new();
    };
    let mut found: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|file| !file.starts_with('.') && fnmatch(name.as_bytes(), file.as_bytes()))
        .map(|file| format!("{dir}/{file}"))
        .collect();
    found.sort();
    found.into_iter().map(String::into_bytes).collect()
}

/// `fnmatch` with `*`, `?` and `[...]`.
fn fnmatch(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => {
            (0..=name.len()).any(|at| fnmatch(rest, name.get(at..).unwrap_or_default()))
        }
        Some((b'?', rest)) => name
            .split_first()
            .is_some_and(|(_, tail)| fnmatch(rest, tail)),
        Some((b'[', rest)) => {
            let Some(close) = rest.iter().position(|b| *b == b']') else {
                return false;
            };
            let (set, after) = (
                rest.get(..close).unwrap_or_default(),
                rest.get(close + 1..).unwrap_or_default(),
            );
            let Some((first, tail)) = name.split_first() else {
                return false;
            };
            let (negated, set) = match set.split_first() {
                Some((b'!' | b'^', set)) => (true, set),
                _ => (false, set),
            };
            let mut hit = false;
            let mut at = 0;
            while at < set.len() {
                let lo = set.get(at).copied().unwrap_or(0);
                if set.get(at + 1) == Some(&b'-') && at + 2 < set.len() {
                    let hi = set.get(at + 2).copied().unwrap_or(0);
                    hit |= (lo..=hi).contains(first);
                    at += 3;
                } else {
                    hit |= lo == *first;
                    at += 1;
                }
            }
            hit != negated && fnmatch(after, tail)
        }
        Some((b, rest)) => name
            .split_first()
            .is_some_and(|(first, tail)| first == b && fnmatch(rest, tail)),
    }
}

/// The scope `FFI::scope` names, laid into this request's arena the
/// first time the request names it; none where nothing preloaded it.
pub(crate) fn scope(req: &mut Request, name: &[u8]) -> Option<Scope> {
    if let Some((_, scope)) = req
        .state::<State>()
        .imported
        .iter()
        .find(|(have, _)| have == name)
    {
        return Some(scope.clone());
    }
    let (types, scope) = {
        let preloads = req.engine::<Preloads>()?;
        if let Some((_, scope)) = preloads.startup.iter().find(|(have, _)| have == name) {
            (
                Source::Startup(Arc::clone(&preloads.types)),
                Arc::clone(scope),
            )
        } else {
            let registered = preloads.registered.get_or_build(name.to_vec(), |_| None);
            (
                Source::Registered(Arc::clone(&registered)),
                Arc::new(Scope::default()),
            )
        }
    };
    let (theirs, scope) = match &types {
        Source::Startup(types) => (types.as_ref(), (*scope).clone()),
        Source::Registered(registered) => {
            let (types, scope) = registered.as_ref().as_ref()?;
            (types, scope.clone())
        }
    };
    let moved = crate::types(req).import(theirs);
    let scope = moved_scope(scope, &moved);
    req.state::<State>()
        .imported
        .push((name.to_vec(), scope.clone()));
    Some(scope)
}

/// Where a preloaded scope's types stand.
enum Source {
    Startup(Arc<Types>),
    Registered(Arc<Option<(Types, Scope)>>),
}

/// A scope's types renumbered as the arena laid them in.
fn moved_scope(mut scope: Scope, moved: &impl Fn(TypeId) -> TypeId) -> Scope {
    if let Some(symbols) = &mut scope.symbols {
        for (_, sym) in symbols.iter_mut() {
            sym.ty = moved(sym.ty);
        }
    }
    if let Some(tags) = &mut scope.tags {
        for (_, tag) in tags.iter_mut() {
            tag.ty = moved(tag.ty);
        }
    }
    scope
}

/// `zend_ffi_load(filename, 1)` while `opcache.preload`'s file is served:
/// the header's scope registered for every request after, its library
/// kept open for the process's life, and the scope this request uses.
/// A failure is php's warning and no scope.
pub(crate) fn register(req: &mut Request, path: &[u8]) -> Option<Scope> {
    let preload = attr::STORED | attr::PERSISTENT;
    let file = header_file(req, path);
    let cwd = req.environment().cwd();
    let loaded = match load_header(&file, Some(&cwd), crate::types(req), preload, None) {
        Ok(loaded) => loaded,
        Err(fail) => {
            for text in preload_warnings(path, &fail) {
                warning(req, &text);
            }
            return None;
        }
    };
    if let Some(lib) = loaded.lib {
        #[expect(
            clippy::mem_forget,
            reason = "a preloaded library stays loaded for the process's life, its symbols named by the scope"
        )]
        std::mem::forget(lib);
    }
    let name = loaded.scope_name.unwrap_or_else(|| b"C".to_vec());
    let held = (types(req).clone(), loaded.scope.clone());
    if let Some(preloads) = req.engine::<Preloads>() {
        // Registered for the process; the entry it answers is not read here.
        drop(preloads.registered.get_or_build(name, |_| Some(held)));
    }
    Some(loaded.scope)
}
