//! The attribute anchor of `witness.coverage` (harness-design.md §4.5
//! condition 2, revision 57; design §7.1, §8). A native is the item an
//! attribute declares: a `fn` under `#[php_function]`, or a method with a
//! body in a `#[php_class]` impl, at any indentation and visibility. A
//! `#[php_class]` on a struct or an enum is the header and declares none; a
//! body-less member, an interface or abstract method or an
//! `implementation =` alias, declares none either.
//!
//! A native is covered by what a fence ran, not by what its text spells
//! (revision 58): a fence of its own, or for a method a fence of its
//! class on the header or on a member impl, whose run through the linked
//! engine entered it. The entry a `php_macros` expansion builds records
//! itself where the request was asked to record it, named as this scan
//! names it: the crate's package, the `#[php_class]` type or none, the
//! identifier. So `new C(`, `foreach`, `(string)`, `[]` and `count()`
//! cover the methods php calls through them, and a fence that names a
//! method it never reaches covers nothing. A crate whose covered sources
//! declare no attribute is counted by [`super::fence::Subject`]'s older
//! sentence, so the anchor moves coverage one crate at a time as natives
//! move onto the attributes; an anchored crate's kernels, the functions of
//! its `ops` module, are still counted by that sentence beside its natives
//! (#1264).
//!
//! The scan is textual over the source with its comments and literals
//! blanked ([`rust::without_comments_or_literals`]), which keeps offsets
//! and lines; the attribute's path is read as written, its last segment
//! naming it (`#[php_function]`, `#[php_ext::attr::php_function]`).
//!
//! **What the build compiled out is no native** (#1068). A `#[cfg(..)]`
//! the target's set decides against ([`cfg::Target`]), on the `fn`, on
//! the impl or the inline `mod` around it, as `#![cfg(..)]` over the file
//! or the block, or on the `mod name;` whose file holds it, takes the item
//! out of the build that links the engine, so no fence can enter it and
//! coverage asks none of it. A `mod name;` is followed to its file by
//! Rust's own layout, `name.rs` or `name/mod.rs` beside the file that
//! declares it and everything under `name/`; a `#[path]` is not read.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use super::fence::{Subject, Witness};
use crate::cfg;
use crate::rust;

/// One native a fence's run entered, as the binary that served it wrote
/// it, `php-linked` beside what the run printed and a host's binary to
/// the file the lane named (`Entered::line`, harness §8, revisions 61 and
/// 84): the crate's package, the `#[php_class]` type or none, the
/// identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Entry {
    pub(crate) krate: String,
    pub(crate) owner: Option<String>,
    pub(crate) name: String,
}

/// What one fence's run of record comes to for coverage: the natives it
/// entered, or that it never got that far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    /// The run of record served the fence, and entered these.
    Entered(Vec<Entry>),
    /// The fence borked before its run of record entered anything: the
    /// oracle did not run it, its server reset the connection or never
    /// answered, or the run of record could not serve it (#1426). The bork
    /// fails the crate in the transport's own words, and what the fence
    /// would have entered is not known, so its subjects are not read as
    /// subjects without a witness: a flake of the transport is a failed
    /// run, not a missing witness.
    Unserved,
}

impl Default for Record {
    fn default() -> Record {
        Record::Entered(Vec::new())
    }
}

impl Entry {
    /// One line as `Entered::line` writes it: three fields a tab apart,
    /// the owner empty for a function. `None` for a line of another shape.
    pub(crate) fn parse(line: &str) -> Option<Entry> {
        let mut fields = line.split('\t');
        let (krate, owner, name) = (fields.next()?, fields.next()?, fields.next()?);
        if fields.next().is_some() || krate.is_empty() || name.is_empty() {
            return None;
        }
        Some(Entry {
            krate: krate.to_owned(),
            owner: (!owner.is_empty()).then(|| owner.to_owned()),
            name: name.to_owned(),
        })
    }
}

/// The attribute on the item below it, as far as coverage reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    Function,
    Class,
}

impl Anchor {
    /// The anchor an attribute's text, `#[` and `]` aside, names.
    fn of(attribute: &str) -> Option<Anchor> {
        let path = attribute
            .split(|c: char| c == '(' || c.is_whitespace())
            .next()
            .unwrap_or_default();
        match path.rsplit("::").next() {
            Some("php_function") => Some(Anchor::Function),
            Some("php_class") => Some(Anchor::Class),
            _ => None,
        }
    }
}

/// What declares a native.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Declared {
    /// A `fn` under `#[php_function]`.
    Function { name: String },
    /// A method with a body in a `#[php_class]` impl of `class`.
    Method { class: String, name: String },
}

impl Declared {
    /// How a subject names it: `fn name`, or `fn Class::name`.
    fn shown(&self) -> String {
        match self {
            Declared::Function { name } => format!("fn {name}"),
            Declared::Method { class, name } => format!("fn {class}::{name}"),
        }
    }

    /// Whether a traced entry is this native: the expansion names a
    /// function with no owner and a method by its impl's type, each by
    /// its identifier with `r#` stripped.
    fn is(&self, entered: &Entry) -> bool {
        match self {
            Declared::Function { name } => entered.owner.is_none() && entered.name == *name,
            Declared::Method { class, name } => {
                entered.owner.as_deref() == Some(class.as_str()) && entered.name == *name
            }
        }
    }
}

/// One native the attributes declare, with the ids of its own fences.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Native {
    subject: Subject,
    declared: Declared,
    own: Vec<String>,
    /// Whether the build that links the engine has it: false under a
    /// `#[cfg]` the target's set denies, which makes it no subject.
    compiled: bool,
}

impl Native {
    /// The native `declared` at the 0-based `line` of `file`, not yet
    /// witnessed.
    fn at(
        file: &Path,
        line: usize,
        declared: Declared,
        own: Vec<String>,
        compiled: bool,
    ) -> Native {
        let subject = Subject {
            file: file.to_path_buf(),
            line: line + 1,
            name: declared.shown(),
            witnessed: false,
        };
        Native {
            subject,
            declared,
            own,
            compiled,
        }
    }
}

/// What the attributes of one crate's covered sources declare.
#[derive(Debug, Clone, Default)]
pub(super) struct Anchored {
    natives: Vec<Native>,
    /// The id of each fence on a `#[php_class]` header or member impl,
    /// beside its class's name.
    class_fences: Vec<(String, String)>,
    /// The path each `mod name;` a `#[cfg]` compiled out would take its
    /// file from, without `.rs`: `name.rs`, and every file under `name/`.
    compiled_out: Vec<PathBuf>,
}

/// A crate whose natives stand on the attributes: what they declare, and
/// the cargo package their entries name themselves by.
#[derive(Debug, Clone)]
pub(crate) struct Coverage {
    pub(super) package: String,
    pub(super) anchored: Anchored,
}

impl Coverage {
    /// The subjects, each witnessed where a fence it may be covered by
    /// entered it, by what each fence's run of record came to keyed by its
    /// id.
    pub(super) fn subjects(&self, entered: &HashMap<String, Record>) -> Vec<Subject> {
        self.anchored.subjects(&Traced {
            package: &self.package,
            entered,
        })
    }
}

/// What each fence's run through the linked engine entered, by the
/// fence's id, for the crate whose package is `package`.
pub(super) struct Traced<'a> {
    pub(super) package: &'a str,
    pub(super) entered: &'a HashMap<String, Record>,
}

impl Traced<'_> {
    /// Whether the fence `id` covers `native` of this crate: its run of
    /// record entered it, or never got far enough to say ([`Record::Unserved`]).
    fn covers(&self, id: &str, native: &Declared) -> bool {
        match self.entered.get(id) {
            Some(Record::Entered(entered)) => entered
                .iter()
                .any(|e| e.krate == self.package && native.is(e)),
            Some(Record::Unserved) => true,
            None => false,
        }
    }
}

impl Anchored {
    /// Take another file's into this one.
    pub(super) fn take(&mut self, other: Anchored) {
        self.natives.extend(other.natives);
        self.class_fences.extend(other.class_fences);
        self.compiled_out.extend(other.compiled_out);
    }

    /// Take the natives of every file a compiled-out `mod name;` would
    /// take out of the build, once every file of the crate has been taken.
    pub(super) fn settle(&mut self) {
        for native in &mut self.natives {
            let file = &native.subject.file;
            if self
                .compiled_out
                .iter()
                .any(|module| file.starts_with(module) || *file == module.with_extension("rs"))
            {
                native.compiled = false;
            }
        }
    }

    /// Whether any native stands on the attributes, so the anchor and not
    /// the older count is what coverage reads; one the build compiled out
    /// stands on them as much as one it has.
    pub(super) fn declares(&self) -> bool {
        !self.natives.is_empty()
    }

    /// Whether a native stands at `line` of `file`, the line of its `fn`,
    /// so the older count does not count it a second time as a kernel.
    pub(super) fn declares_at(&self, file: &Path, line: usize) -> bool {
        self.natives
            .iter()
            .any(|native| native.subject.file == file && native.subject.line == line)
    }

    /// The subjects coverage counts, one per native the build has, each
    /// witnessed where a fence of its own, or for a method one of its
    /// class's, entered it when the linked engine ran the fence.
    pub(super) fn subjects(&self, traced: &Traced<'_>) -> Vec<Subject> {
        self.natives
            .iter()
            .filter(|native| native.compiled)
            .map(|native| {
                let mut subject = native.subject.clone();
                let class = match &native.declared {
                    Declared::Method { class, .. } => Some(class),
                    Declared::Function { .. } => None,
                };
                let eligible = native.own.iter().chain(
                    self.class_fences
                        .iter()
                        .filter(|(of, _)| Some(of) == class)
                        .map(|(_, id)| id),
                );
                subject.witnessed = eligible
                    .into_iter()
                    .any(|id| traced.covers(id, &native.declared));
                subject
            })
            .collect()
    }
}

/// Every native the attributes in one file declare for `target`, and
/// every fence on a class; `witnesses` are the file's own, as
/// [`super::fence::extract`] read them.
pub(super) fn scan(
    file: &Path,
    text: &str,
    witnesses: &[Witness],
    target: &cfg::Target,
) -> Anchored {
    let code = rust::without_comments_or_literals(text);
    let source: Vec<&str> = text.lines().collect();
    // The witnesses on an item, by the `php` fence's 0-based line; a `//!`
    // block's belong to the file and to no item.
    let on_an_item: Vec<(usize, &Witness)> = witnesses
        .iter()
        .map(|w| (w.line - 1, w))
        .filter(|(at, _)| {
            source
                .get(*at)
                .is_some_and(|line| line.trim_start().starts_with("///"))
        })
        .collect();
    let mut out = Anchored::default();
    let mut scanner = Scanner::new(&code);
    // The line of the last `{`, `}` or `;`: a doc block after it is the
    // next item's; nothing before the first.
    let mut since: Option<usize> = None;
    let mut pending: Option<Anchor> = None;
    // The member depth and the name of each `#[php_class]` impl open here.
    let mut classes: Vec<(usize, String)> = Vec::new();
    let mut build = Build::default();
    let mut depth = 0usize;
    while let Some(token) = scanner.next() {
        match token {
            Token::Attribute { at, inner } => {
                let blanked = build.attribute(target, (text, &code), at, inner, depth);
                if !inner {
                    pending = Anchor::of(blanked).or(pending);
                }
            }
            Token::Punct(c) => {
                since = Some(scanner.line);
                pending = None;
                match c {
                    '{' => {
                        depth += 1;
                        build.open(depth);
                    }
                    '}' => {
                        depth = depth.saturating_sub(1);
                        classes.retain(|(members, _)| *members <= depth);
                        build.close(depth);
                    }
                    _ => out.compiled_out.extend(build.end(file)),
                }
            }
            Token::Word(word) => {
                let compiled = build.has();
                let line = scanner.line;
                let own = || {
                    on_an_item
                        .iter()
                        .filter(move |(at, _)| since.is_none_or(|s| *at > s) && *at < line)
                        .map(|(_, w)| *w)
                };
                match word {
                    "fn" => {
                        let name = scanner.identifier().unwrap_or_default();
                        let name = name.strip_prefix("r#").unwrap_or(name).to_owned();
                        let member = classes.last().filter(|(members, _)| *members == depth);
                        let declared = match (pending, member) {
                            (Some(Anchor::Function), _) => Some(Declared::Function { name }),
                            (_, Some((_, class))) if scanner.has_body() => Some(Declared::Method {
                                class: class.clone(),
                                name,
                            }),
                            _ => None,
                        };
                        if let Some(declared) = declared {
                            let own = own().map(Witness::id).collect();
                            out.natives
                                .push(Native::at(file, line, declared, own, compiled));
                        }
                        pending = None;
                    }
                    "impl" | "struct" | "enum" if pending == Some(Anchor::Class) => {
                        let class = if word == "impl" {
                            scanner.self_type()
                        } else {
                            scanner.identifier().unwrap_or_default().to_owned()
                        };
                        if compiled {
                            out.class_fences
                                .extend(own().map(|w| (class.clone(), w.id())));
                        }
                        if word == "impl" {
                            classes.push((depth + 1, class));
                        }
                        pending = None;
                    }
                    "mod" => {
                        build.module = scanner.identifier().map(str::to_owned);
                        pending = None;
                    }
                    "impl" | "struct" | "enum" | "union" | "trait" | "type" | "static" | "use"
                    | "macro_rules" => pending = None,
                    _ => {}
                }
            }
        }
    }
    out
}

/// What of a file the build has, as [`scan`] walks it (#1068).
#[derive(Debug, Default)]
struct Build {
    /// The member depth and the name of each inline `mod` open here.
    mods: Vec<(usize, String)>,
    /// The `mod` whose name was read and whose `{` or `;` is to come.
    module: Option<String>,
    /// Whether a `#[cfg]` since the last `{`, `}` or `;` is false for the
    /// target, which takes the item it stands on out of the build.
    denied: bool,
    /// The depth from which the build has none of what stands here: a
    /// block whose item a `#[cfg]` denied, or one an inner `#![cfg]` did.
    out_from: Option<usize>,
}

impl Build {
    /// The attribute `at` spans in the blanked source of `(text, code)`,
    /// at `depth`, which `target` admits or not: a denied outer one takes
    /// the item below it out of the build, an inner one the block or the
    /// file it stands in. Only a `cfg` is read with its literals, from the
    /// source. The blanked text is what it answers.
    fn attribute<'c>(
        &mut self,
        target: &cfg::Target,
        (text, code): (&str, &'c str),
        at: Range<usize>,
        inner: bool,
        depth: usize,
    ) -> &'c str {
        let blanked = code.get(at.clone()).unwrap_or_default().trim();
        if !blanked.starts_with("cfg") || target.admits(unblanked(text, code, at).trim()) {
            return blanked;
        }
        if inner {
            self.out_from = Some(self.out_from.map_or(depth, |d| d.min(depth)));
        } else {
            self.denied = true;
        }
        blanked
    }

    /// A `{` that took the depth to `depth`.
    fn open(&mut self, depth: usize) {
        if self.denied && self.out_from.is_none() {
            self.out_from = Some(depth);
        }
        if let Some(name) = self.module.take() {
            self.mods.push((depth, name));
        }
        self.denied = false;
    }

    /// A `}` that took the depth back to `depth`.
    fn close(&mut self, depth: usize) {
        self.mods.retain(|(members, _)| *members <= depth);
        if self.out_from.is_some_and(|d| depth < d) {
            self.out_from = None;
        }
        self.denied = false;
    }

    /// A `;`: where it ends a `mod name;` the build does not take, the
    /// path of `file`'s that the module's file would stand at.
    fn end(&mut self, file: &Path) -> Option<PathBuf> {
        let name = self.module.take();
        let out = self.denied || self.out_from.is_some();
        self.denied = false;
        let name = name.filter(|_| out)?;
        let mut path = module_dir(file);
        path.extend(self.mods.iter().map(|(_, m)| m.as_str()));
        path.push(name);
        Some(path)
    }

    /// Whether the build has what stands at the cursor.
    fn has(&self) -> bool {
        !self.denied && self.out_from.is_none()
    }
}

/// The stretch of `text` that `at` spans in `code`, its blanked copy: the
/// blanker keeps each character's place and not its width, so the offsets
/// are carried over by characters.
fn unblanked<'a>(text: &'a str, code: &str, at: Range<usize>) -> &'a str {
    let chars = |offset: usize| code.get(..offset).map_or(0, |c| c.chars().count());
    let byte = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(b, _)| b);
    let (start, end) = (byte(chars(at.start)), byte(chars(at.end)));
    text.get(start..end).unwrap_or_default()
}

/// The directory a file's `mod name;` takes `name` from: the file's own
/// for a crate root or a `mod.rs`, and `stem/` beside it for any other.
fn module_dir(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or(Path::new("")).to_path_buf();
    match file.file_stem().and_then(|s| s.to_str()) {
        Some("lib" | "main" | "mod") | None => parent,
        Some(stem) => parent.join(stem),
    }
}

/// What the scanner reads off the blanked source.
enum Token<'a> {
    /// An attribute's text between `#[` (or `#![`) and its `]`, by its
    /// offsets in the blanked source.
    Attribute { at: Range<usize>, inner: bool },
    /// `{`, `}` or `;`.
    Punct(char),
    /// An identifier or a keyword.
    Word(&'a str),
}

/// A cursor over the blanked source that knows its 0-based line.
struct Scanner<'a> {
    code: &'a str,
    at: usize,
    line: usize,
}

impl<'a> Scanner<'a> {
    fn new(code: &'a str) -> Scanner<'a> {
        Scanner {
            code,
            at: 0,
            line: 0,
        }
    }

    fn rest(&self) -> &'a str {
        &self.code[self.at..]
    }

    /// Move `by` bytes on, counting the lines passed.
    fn advance(&mut self, by: usize) {
        let end = (self.at + by).min(self.code.len());
        self.line += self.code[self.at..end].matches('\n').count();
        self.at = end;
    }

    fn skip_space(&mut self) {
        let rest = self.rest();
        let by = rest.len() - rest.trim_start().len();
        self.advance(by);
    }

    /// The identifier at the cursor, after any space, consumed.
    fn identifier(&mut self) -> Option<&'a str> {
        self.skip_space();
        let found = super::fence::identifier(self.rest())?;
        self.advance(found.len());
        Some(found)
    }

    /// The offset past the bracket that closes the one opening at `from`,
    /// each of `opens` opening one and each of `closes` closing one.
    fn closing(text: &str, from: usize, (opens, closes): (&[char], &[char])) -> usize {
        let mut depth = 0usize;
        for (i, c) in text[from..].char_indices() {
            if opens.contains(&c) {
                depth += 1;
            } else if closes.contains(&c) {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return from + i + 1;
                }
            }
        }
        text.len()
    }

    /// Whether the `fn` whose name was just read has a body: the first
    /// `{` or `;` outside its parentheses and brackets is a `{`. The
    /// cursor does not move.
    fn has_body(&self) -> bool {
        let mut depth = 0usize;
        for c in self.rest().chars() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                '{' if depth == 0 => return true,
                ';' if depth == 0 => return false,
                _ => {}
            }
        }
        false
    }

    /// The self type of the impl whose keyword was just read, by its last
    /// path segment: `impl<T> Trait for a::Name<T>` is `Name`. The cursor
    /// does not move.
    fn self_type(&self) -> String {
        let rest = self.rest();
        let head = &rest[..rest.find('{').unwrap_or(rest.len())];
        let head = head.trim_start();
        let head = if head.starts_with('<') {
            &head[Self::closing(head, 0, (&['<'], &['>']))..]
        } else {
            head
        };
        let target = head.rsplit_once(" for ").map_or(head, |(_, t)| t);
        let target = target.split('<').next().unwrap_or_default();
        target
            .trim()
            .rsplit("::")
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

impl<'a> Iterator for Scanner<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        loop {
            self.skip_space();
            let rest = self.rest();
            let c = rest.chars().next()?;
            if rest.starts_with("#[") || rest.starts_with("#![") {
                let open = rest.find('[').unwrap_or_default();
                let end = Self::closing(rest, open, (&['[', '('], &[']', ')']));
                let inner = rest.starts_with("#!");
                let at = self.at + open + 1..self.at + end.saturating_sub(1).max(open + 1);
                self.advance(end);
                return Some(Token::Attribute { at, inner });
            }
            if matches!(c, '{' | '}' | ';') {
                self.advance(1);
                return Some(Token::Punct(c));
            }
            if c.is_alphabetic() || c == '_' {
                return self.identifier().map(Token::Word);
            }
            self.advance(c.len_utf8());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::witness::fence;

    const PACKAGE: &str = "php_ext_fixture";

    /// A native as the fixture crate's expansion would record it.
    fn native(owner: Option<&str>, name: &str) -> Entry {
        Entry {
            krate: PACKAGE.to_owned(),
            owner: owner.map(str::to_owned),
            name: name.to_owned(),
        }
    }

    /// The subjects of `text`, each fence's run having entered what
    /// `trace` gives it: the fence whose snippet contains the first half.
    fn subjects(text: &str, trace: &[(&str, Vec<Entry>)]) -> Option<Vec<Subject>> {
        let file = Path::new("ext/fixture/src/lib.rs");
        let found = fence::extract(file, text);
        let anchored = scan(file, text, &found.witnesses, cfg::Target::built());
        if !anchored.declares() {
            return None;
        }
        let entered: HashMap<String, Record> = found
            .witnesses
            .iter()
            .map(|w| {
                let natives = trace
                    .iter()
                    .filter(|(marker, _)| w.snippet.contains(marker))
                    .flat_map(|(_, natives)| natives.iter().cloned())
                    .collect();
                (w.id(), Record::Entered(natives))
            })
            .collect();
        let coverage = Coverage {
            package: PACKAGE.to_owned(),
            anchored,
        };
        Some(coverage.subjects(&entered))
    }

    fn shown(found: &[Subject]) -> Vec<(&str, bool)> {
        found
            .iter()
            .map(|s| (s.name.as_str(), s.witnessed))
            .collect()
    }

    /// The line a binary writes reads back as the entry it names, and a
    /// line of another shape reads as none.
    #[test]
    fn a_line_the_binary_appended_is_the_entry_it_names() {
        let method = native(Some("C"), "count");
        let line = format!("{}\t{}\t{}", method.krate, "C", method.name);
        assert_eq!(Entry::parse(&line), Some(method));
        assert_eq!(
            Entry::parse("php_ext_fixture\t\tnative"),
            Some(native(None, "native"))
        );
        assert_eq!(Entry::parse("php_ext_fixture\tnative"), None);
    }

    const WITNESS: &str = "/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\n";

    #[test]
    fn a_file_with_no_attribute_leaves_the_older_count_standing() {
        assert!(subjects(&format!("{WITNESS}pub fn native() {{}}\n"), &[]).is_none());
    }

    #[test]
    fn a_function_is_covered_by_its_own_fence_where_the_run_entered_it() {
        let text = format!(
            "mod inner {{\n    {}    #[php_ext::attr::php_function]\n    pub(crate) fn witnessed() {{}}\n    #[php_function(arity = manual)]\n    fn bare() {{}}\n    {}    #[php_function]\n    fn r#type() {{}}\n}}\npub fn helper() {{}}\n",
            WITNESS.replace("///", "    ///"),
            WITNESS
                .replace("echo 1", "echo 2")
                .replace("///", "    ///"),
        );
        let trace = [
            ("echo 1", vec![native(None, "witnessed")]),
            ("echo 2", vec![native(None, "type")]),
        ];
        let found = subjects(&text, &trace).expect("the attribute declares natives");
        assert_eq!(
            shown(&found),
            [
                ("fn witnessed", true),
                ("fn bare", false),
                ("fn type", true)
            ]
        );
        assert_eq!(found[1].line, 11);
        let untraced = subjects(&text, &[]).expect("the attribute declares natives");
        assert!(
            untraced.iter().all(|s| !s.witnessed),
            "a fence of its own that did not enter it covers nothing"
        );
    }

    #[test]
    fn a_class_fence_covers_what_its_run_entered_and_nothing_it_only_names() {
        // The constructor is reached through `new C(` alone, and `never`
        // is named by the fence, which throws before reaching it.
        let text = "/// ```php\n/// $c = new C(1); $c->used(1); throw new Exception; $c->never();\n/// ```\n/// ```expect\n/// ```\n#[php_class]\npub struct C;\n\n#[php_class]\nimpl C {\n    const X: i64 = 1;\n    fn __construct(this: This<Self>, n: i64) { let s = \"}\"; }\n    fn used(this: This<Self>, n: i64) -> i64 { n }\n    fn never(this: This<Self>) {}\n    fn alias(this: This<Self>);\n}\n";
        let trace = [(
            "new C(1)",
            vec![native(Some("C"), "__construct"), native(Some("C"), "used")],
        )];
        let found = subjects(text, &trace).expect("the attribute declares natives");
        assert_eq!(
            shown(&found),
            [
                ("fn C::__construct", true),
                ("fn C::used", true),
                ("fn C::never", false)
            ]
        );
    }

    #[test]
    fn a_method_entered_only_by_another_class_or_another_crate_is_not_covered() {
        let text = "/// ```php\n/// new A;\n/// ```\n/// ```expect\n/// ```\n#[php_class]\nimpl A {\n    fn go(this: This<Self>) {}\n}\n#[php_class]\nimpl B {\n    fn go(this: This<Self>) {}\n}\n";
        let elsewhere = Entry {
            krate: "php_ext_other".to_owned(),
            owner: Some("A".to_owned()),
            name: "go".to_owned(),
        };
        let trace = [("new A", vec![native(Some("B"), "go"), elsewhere])];
        let found = subjects(text, &trace).expect("the attribute declares natives");
        assert_eq!(shown(&found), [("fn A::go", false), ("fn B::go", false)]);
    }

    #[test]
    fn a_method_carries_its_own_witness_and_a_header_alone_declares_nothing() {
        let text = format!(
            "#[php_class(extends = Base)]\npub enum Suit {{ Hearts }}\n#[php_class]\nimpl<'a> Trait for crate::Suit {{\n{}    fn label(this: This<Self>) {{}}\n}}\n",
            WITNESS.replace("///", "    ///")
        );
        let trace = [("echo 1", vec![native(Some("Suit"), "label")])];
        let found = subjects(&text, &trace).expect("the attribute declares natives");
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].name, "fn Suit::label");
        assert!(found[0].witnessed);
        assert!(
            subjects("#[php_class]\npub struct Only;\n", &[]).is_none(),
            "a header declares no native"
        );
    }
}
