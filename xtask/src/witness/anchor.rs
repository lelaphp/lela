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
//! itself under `php_engine`'s `witness-trace` feature, named as this scan
//! names it: the crate's package, the `#[php_class]` type or none, the
//! identifier. So `new C(`, `foreach`, `(string)`, `[]` and `count()`
//! cover the methods php calls through them, and a fence that names a
//! method it never reaches covers nothing. A crate whose covered sources
//! declare no attribute is counted by [`super::fence::Subject`]'s older
//! sentence, so the anchor moves coverage one crate at a time as natives
//! move onto the attributes.
//!
//! The scan is textual over the source with its comments and literals
//! blanked ([`rust::without_comments_or_literals`]), which keeps offsets
//! and lines; the attribute's path is read as written, its last segment
//! naming it (`#[php_function]`, `#[php_ext::attr::php_function]`).

use std::collections::HashMap;
use std::path::Path;

use php_bundle::test::Entered;

use super::fence::{Subject, Witness};
use crate::rust;

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
    /// Whether a traced entry is this native: the expansion names a
    /// function with no owner and a method by its impl's type, each by
    /// its identifier with `r#` stripped.
    fn is(&self, entered: &Entered) -> bool {
        match self {
            Declared::Function { name } => entered.owner.is_none() && entered.name == name,
            Declared::Method { class, name } => {
                entered.owner == Some(class.as_str()) && entered.name == name
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
}

/// What the attributes of one crate's covered sources declare.
#[derive(Debug, Clone, Default)]
pub(super) struct Anchored {
    natives: Vec<Native>,
    /// The id of each fence on a `#[php_class]` header or member impl,
    /// beside its class's name.
    class_fences: Vec<(String, String)>,
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
    /// entered it, by what each fence's run entered keyed by its id.
    pub(super) fn subjects(&self, entered: &HashMap<String, Vec<Entered>>) -> Vec<Subject> {
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
    pub(super) entered: &'a HashMap<String, Vec<Entered>>,
}

impl Traced<'_> {
    /// Whether the fence `id` entered `native` of this crate.
    fn entered(&self, id: &str, native: &Declared) -> bool {
        self.entered.get(id).is_some_and(|entered| {
            entered
                .iter()
                .any(|e| e.krate == self.package && native.is(e))
        })
    }
}

impl Anchored {
    /// Take another file's into this one.
    pub(super) fn take(&mut self, other: Anchored) {
        self.natives.extend(other.natives);
        self.class_fences.extend(other.class_fences);
    }

    /// Whether any native stands on the attributes, so the anchor and not
    /// the older count is what coverage reads.
    pub(super) fn declares(&self) -> bool {
        !self.natives.is_empty()
    }

    /// The subjects coverage counts, one per native, each witnessed where
    /// a fence of its own, or for a method one of its class's, entered it
    /// when the linked engine ran the fence.
    pub(super) fn subjects(&self, traced: &Traced<'_>) -> Vec<Subject> {
        self.natives
            .iter()
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
                    .any(|id| traced.entered(id, &native.declared));
                subject
            })
            .collect()
    }
}

/// Every native the attributes in one file declare, and every fence on a
/// class; `witnesses` are the file's own, as [`super::fence::extract`]
/// read them.
pub(super) fn scan(file: &Path, text: &str, witnesses: &[Witness]) -> Anchored {
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
    let mut depth = 0usize;
    while let Some(token) = scanner.next() {
        match token {
            Token::Attribute(text) => pending = Anchor::of(text).or(pending),
            Token::Punct(c) => {
                since = Some(scanner.line);
                pending = None;
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth = depth.saturating_sub(1);
                        classes.retain(|(members, _)| *members <= depth);
                    }
                    _ => {}
                }
            }
            Token::Word(word) => {
                let line = scanner.line;
                let own = || {
                    on_an_item
                        .iter()
                        .filter(move |(at, _)| since.is_none_or(|s| *at > s) && *at < line)
                        .map(|(_, w)| *w)
                };
                match word {
                    "fn" => {
                        let name = scanner.identifier().unwrap_or_default().to_owned();
                        let member = classes.last().filter(|(members, _)| *members == depth);
                        let name = name.strip_prefix("r#").unwrap_or(&name).to_owned();
                        let declared = match (pending, member) {
                            (Some(Anchor::Function), _) => {
                                Some(Declared::Function { name: name.clone() })
                            }
                            (_, Some((_, class))) if scanner.has_body() => Some(Declared::Method {
                                class: class.clone(),
                                name: name.clone(),
                            }),
                            _ => None,
                        };
                        if let Some(declared) = declared {
                            let shown = match &declared {
                                Declared::Function { .. } => format!("fn {name}"),
                                Declared::Method { class, .. } => format!("fn {class}::{name}"),
                            };
                            out.natives.push(Native {
                                subject: Subject {
                                    file: file.to_path_buf(),
                                    line: line + 1,
                                    name: shown,
                                    witnessed: false,
                                },
                                declared,
                                own: own().map(Witness::id).collect(),
                            });
                        }
                        pending = None;
                    }
                    "impl" | "struct" | "enum" if pending == Some(Anchor::Class) => {
                        let class = if word == "impl" {
                            scanner.self_type()
                        } else {
                            scanner.identifier().unwrap_or_default().to_owned()
                        };
                        out.class_fences
                            .extend(own().map(|w| (class.clone(), w.id())));
                        if word == "impl" {
                            classes.push((depth + 1, class));
                        }
                        pending = None;
                    }
                    "impl" | "struct" | "enum" | "union" | "trait" | "type" | "mod" | "static"
                    | "use" | "macro_rules" => pending = None,
                    _ => {}
                }
            }
        }
    }
    out
}

/// What the scanner reads off the blanked source.
enum Token<'a> {
    /// An outer attribute's text between `#[` and its `]`.
    Attribute(&'a str),
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
        let rest = self.rest();
        let raw = if rest.starts_with("r#") { 2 } else { 0 };
        let len = rest[raw..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(rest.len(), |n| raw + n);
        (len > raw).then(|| {
            self.advance(len);
            &rest[..len]
        })
    }

    /// The offset past the bracket that closes the one opening at `from`.
    fn closing(text: &str, from: usize) -> usize {
        let mut depth = 0usize;
        for (i, c) in text[from..].char_indices() {
            match c {
                '[' | '(' => depth += 1,
                ']' | ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return from + i + 1;
                    }
                }
                _ => {}
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
            &head[Self::angle_close(head)..]
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

    /// The offset past the `>` that closes the `<` at the start of `text`.
    fn angle_close(text: &str) -> usize {
        let mut depth = 0usize;
        for (i, c) in text.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
        }
        text.len()
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
                let end = Self::closing(rest, open);
                let inner = rest.starts_with("#[");
                self.advance(end);
                if inner {
                    let text = rest[open + 1..end.saturating_sub(1).max(open + 1)].trim();
                    return Some(Token::Attribute(text));
                }
                continue;
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
    fn native(owner: Option<&'static str>, name: &'static str) -> Entered {
        Entered {
            krate: PACKAGE,
            owner,
            name,
        }
    }

    /// The subjects of `text`, each fence's run having entered what
    /// `trace` gives it: the fence whose snippet contains the first half.
    fn subjects(text: &str, trace: &[(&str, Vec<Entered>)]) -> Option<Vec<Subject>> {
        let file = Path::new("ext/fixture/src/lib.rs");
        let found = fence::extract(file, text);
        let anchored = scan(file, text, &found.witnesses);
        if !anchored.declares() {
            return None;
        }
        let entered: HashMap<String, Vec<Entered>> = found
            .witnesses
            .iter()
            .map(|w| {
                let natives = trace
                    .iter()
                    .filter(|(marker, _)| w.snippet.contains(marker))
                    .flat_map(|(_, natives)| natives.iter().copied())
                    .collect();
                (w.id(), natives)
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
        let elsewhere = Entered {
            krate: "php_ext_other",
            owner: Some("A"),
            name: "go",
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
