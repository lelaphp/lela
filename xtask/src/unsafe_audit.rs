//! `xtask unsafe-audit` (design §4.1, §10; harness-design.md §6): `unsafe`
//! is permitted in the homes the manifest's `unsafe.homes` rule lists and
//! nowhere else. Every other crate is `#![forbid(unsafe_code)]` at its
//! root; a crate whose home is one module is `#![deny(unsafe_code)]` with
//! the allowance on that module alone. One reading per crate.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::git::Stamp;
use crate::lines;
use crate::manifest::{Crate, Manifest};
use crate::reading::{Reading, Status};
use crate::sources;

/// The rule the audit reads.
pub const RULE: &str = "unsafe.homes";

/// Audit every created crate; one reading each.
pub fn run(root: &Path, manifest: &Manifest) -> Result<Vec<Reading>> {
    let rule = manifest
        .rule(RULE)
        .with_context(|| format!("the manifest has no rule {RULE}"))?;
    let homes: Vec<Home> = rule
        .predicate
        .allow
        .iter()
        .flatten()
        .map(|h| Home::parse(h))
        .collect::<Result<_>>()?;
    let mut readings = Vec::new();
    for krate in &manifest.crates {
        let mut reading = Reading::new("unsafe-audit", &krate.name);
        RULE.clone_into(&mut reading.rule);
        reading.adr = rule.adr.adr_field();
        if sources::exists(root, krate) {
            let audit = audit(root, krate, &homes)?;
            reading.count("sites", u64::try_from(audit.sites)?)?;
            reading.count("outside", u64::try_from(audit.outside.len())?)?;
            if audit.outside.is_empty() && audit.attribute.is_none() {
                reading.status = Status::Pass;
                reading.message = format!(
                    "{}: {} unsafe sites, all in the homes {RULE} allows",
                    krate.name, audit.sites
                );
            } else {
                reading.status = Status::Fail;
                let mut lines = audit.outside;
                lines.extend(audit.attribute);
                reading.message = format!(
                    "{}. {} {}",
                    lines.join("; "),
                    rule.text,
                    rule.adr.adr_cited()
                );
            }
            reading.stamp(&Stamp::take(root, &krate.path())?);
        } else {
            reading.message = format!("{}: not yet created", krate.name);
        }
        readings.push(reading);
    }
    Ok(readings)
}

/// What the audit found in one crate.
#[derive(Debug, Default)]
pub struct Audit {
    /// Every `unsafe` site in the crate's sources.
    pub sites: usize,
    /// The sites outside the homes, as `path:line`.
    pub outside: Vec<String>,
    /// What is wrong with the crate root's `unsafe_code` attribute, if anything.
    pub attribute: Option<String>,
}

/// Audit one crate's sources against the homes.
pub fn audit(root: &Path, krate: &Crate, homes: &[Home]) -> Result<Audit> {
    let mut found = Audit::default();
    let whole = homes.iter().any(|h| h.covers_whole(krate));
    let allowed = |path: &str| homes.iter().any(|h| h.covers(krate, path));
    let sources = sources::rust(root, krate)?;
    // A crate with a module home (`php_jit::enter`), or with sources under
    // a path home (an extension with bindings), is `deny` with the
    // allowance on that module; every other crate is `forbid`.
    let modules = homes.iter().any(|h| h.is_module_of(krate))
        || sources.iter().any(|p| {
            homes
                .iter()
                .any(|h| matches!(h, Home::Path(_)) && h.covers(krate, p))
        });
    for path in sources {
        let text =
            std::fs::read_to_string(root.join(&path)).with_context(|| format!("reading {path}"))?;
        for (line, code) in lines::code(&text, false) {
            let sites = unsafe_sites(&code);
            found.sites += sites;
            if sites > 0 && !allowed(&path) {
                found
                    .outside
                    .push(format!("{path}:{line}: unsafe outside the homes"));
            }
        }
    }
    if !whole {
        let want = if modules { "deny" } else { "forbid" };
        for name in ["lib.rs", "main.rs"] {
            let path = krate.path().join("src").join(name);
            let Ok(text) = std::fs::read_to_string(root.join(&path)) else {
                continue;
            };
            let has = lines::code(&text, false)
                .iter()
                .any(|(_, code)| code.contains(&format!("#![{want}(unsafe_code)]")));
            if !has {
                found.attribute = Some(format!(
                    "{}: the crate root lacks #![{want}(unsafe_code)]",
                    path.display()
                ));
            }
        }
    }
    Ok(found)
}

/// The `unsafe` keywords on one line of code: `unsafe {`, `unsafe fn`,
/// `unsafe impl`, `unsafe trait`, `unsafe extern`; `unsafe_code` in an
/// attribute is not one, and neither is the word inside a string literal.
pub fn unsafe_sites(code: &str) -> usize {
    let code = without_strings(code);
    let bytes = code.as_bytes();
    let mut count = 0;
    let mut from = 0;
    while let Some(at) = code[from..].find("unsafe") {
        let start = from + at;
        let end = start + "unsafe".len();
        let word = |i: usize| {
            bytes
                .get(i)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        };
        let preceded = start > 0 && word(start - 1);
        if !preceded && !word(end) {
            count += 1;
        }
        from = end;
    }
    count
}

/// The line with every `"..."` literal blanked, escapes honoured, so a
/// word in a string is not a keyword. A literal that runs past the line
/// is blanked to its end.
fn without_strings(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            out.push(c);
            continue;
        }
        out.push(' ');
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => break,
                _ => {}
            }
        }
    }
    out
}

/// One entry of `unsafe.homes`: a crate, a path pattern, or a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Home {
    /// The whole crate: `php_heap`.
    Crate(String),
    /// A directory pattern over source paths: `ext/*/src/ffi/`.
    Path(Vec<String>),
    /// One module of a crate: `php_jit::table::code`, the file
    /// `src/table/code.rs` or the directory `src/table/code/`.
    Module { krate: String, module: Vec<String> },
}

impl Home {
    pub fn parse(text: &str) -> Result<Home> {
        if text.contains('/') {
            let parts: Vec<String> = text
                .trim_end_matches('/')
                .split('/')
                .map(str::to_owned)
                .collect();
            if parts.iter().any(String::is_empty) {
                bail!("unsafe home {text:?} has an empty path segment");
            }
            return Ok(Home::Path(parts));
        }
        let mut parts = text.split("::").map(str::to_owned);
        let krate = parts.next().unwrap_or_default();
        let module: Vec<String> = parts.collect();
        if krate.is_empty() || module.iter().any(String::is_empty) {
            bail!("unsafe home {text:?} is neither a crate, a path nor a module");
        }
        Ok(if module.is_empty() {
            Home::Crate(krate)
        } else {
            Home::Module { krate, module }
        })
    }

    fn covers_whole(&self, krate: &Crate) -> bool {
        matches!(self, Home::Crate(name) if *name == krate.name)
    }

    fn is_module_of(&self, krate: &Crate) -> bool {
        matches!(self, Home::Module { krate: name, .. } if *name == krate.name)
    }

    /// Whether the source at `path` (relative to the root) is in this home.
    pub fn covers(&self, krate: &Crate, path: &str) -> bool {
        match self {
            Home::Crate(name) => *name == krate.name,
            Home::Path(pattern) => {
                let parts: Vec<&str> = path.split('/').collect();
                parts.len() > pattern.len()
                    && pattern
                        .iter()
                        .zip(&parts)
                        .all(|(want, got)| want == "*" || want == got)
            }
            Home::Module {
                krate: name,
                module,
            } => {
                if *name != krate.name {
                    return false;
                }
                let base = krate.path().join("src").join(module.join("/"));
                let file = base.with_extension("rs");
                let path = Path::new(path);
                path == file || path.starts_with(&base)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Home, unsafe_sites};
    use crate::manifest::Crate;

    fn krate(name: &str) -> Crate {
        Crate {
            name: name.to_owned(),
            budget: 1,
            lock: crate::manifest::OneOrMany::None,
            lock_from: std::collections::BTreeMap::new(),
            feature: None,
            targets: Vec::new(),
            phpt: Vec::new(),
        }
    }

    #[test]
    fn the_keyword_is_a_site_and_the_attribute_word_is_not() {
        assert_eq!(unsafe_sites("#![forbid(unsafe_code)]"), 0);
        assert_eq!(unsafe_sites("unsafe { a(); } unsafe fn b()"), 2);
        assert_eq!(unsafe_sites("let is_unsafe = 1;"), 0);
        assert_eq!(
            unsafe_sites("let s = \"unsafe { \\\" unsafe\"; unsafe {}"),
            1
        );
    }

    #[test]
    fn the_four_home_shapes_cover_what_they_name() {
        let heap = Home::parse("php_heap").unwrap();
        assert!(heap.covers(&krate("php_heap"), "crates/php_heap/src/lib.rs"));
        assert!(!heap.covers(&krate("php_unit"), "crates/php_unit/src/lib.rs"));

        let ffi = Home::parse("ext/*/src/ffi/").unwrap();
        assert!(ffi.covers(&krate("ext/pcre"), "ext/pcre/src/ffi/mod.rs"));
        assert!(ffi.covers(&krate("ext/pcre"), "ext/pcre/src/ffi/deep/x.rs"));
        assert!(!ffi.covers(&krate("ext/pcre"), "ext/pcre/src/lib.rs"));

        let code = Home::parse("php_jit::table::code").unwrap();
        assert!(code.covers(&krate("php_jit"), "crates/php_jit/src/table/code.rs"));
        assert!(code.covers(&krate("php_jit"), "crates/php_jit/src/table/code/pages.rs"));
        assert!(!code.covers(&krate("php_jit"), "crates/php_jit/src/table/mod.rs"));
        assert!(!code.covers(&krate("php_jit"), "crates/php_jit/src/table/codec.rs"));
    }
}
