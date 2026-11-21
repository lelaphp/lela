//! A runner's XML log, read as its cases: the format `phpunit`'s
//! `--log-junit` and Pest's write. Only what the lane reads is read, each
//! `<testcase>`'s name and class and whether a `<failure>`, `<error>` or
//! `<skipped>` stands inside it; the suites around them, the times and
//! the output are passed over, and comments and character data are never
//! read as elements. A log a crash cut short reports the cases it closed,
//! and the one left open is not reported.

/// One case a log reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    /// `<class>::<name>`; the case's file stands for its class where it
    /// names none, and its name alone is its id where it names neither.
    pub id: String,
    pub outcome: Outcome,
    /// The first line of what its failure, error or skip said.
    pub message: String,
}

/// How a case came out, the worst last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Passed,
    Skipped,
    Failed,
    Errored,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Skipped => "skipped",
            Outcome::Failed => "failed",
            Outcome::Errored => "errored",
        }
    }
}

/// The cases `xml` reports, in its order.
pub fn cases(xml: &str) -> Vec<Case> {
    let mut found = Vec::new();
    let mut open: Option<Case> = None;
    let mut rest = xml;
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        let passed = [("![CDATA[", "]]>"), ("!--", "-->"), ("?", "?>")]
            .into_iter()
            .find_map(|(opens, closes)| {
                let inside = rest.strip_prefix(opens)?;
                Some(
                    inside
                        .find(closes)
                        .map_or("", |end| &inside[end + closes.len()..]),
                )
            });
        if let Some(after) = passed {
            rest = after;
            continue;
        }
        let Some(end) = tag_end(rest) else { break };
        let (tag, after) = (&rest[..end], &rest[end + 1..]);
        rest = after;
        let closing = tag.starts_with('/');
        let body = tag.trim_start_matches('/').trim_end_matches('/');
        let name = body.split(|c: char| c.is_whitespace()).next().unwrap_or("");
        match (closing, name) {
            (false, "testcase") => {
                found.extend(open.take());
                let case = case(body);
                if tag.ends_with('/') {
                    found.push(case);
                } else {
                    open = Some(case);
                }
            }
            (true, "testcase") => found.extend(open.take()),
            (false, "failure" | "error" | "skipped") => {
                if let Some(case) = &mut open {
                    let outcome = match name {
                        "failure" => Outcome::Failed,
                        "error" => Outcome::Errored,
                        _ => Outcome::Skipped,
                    };
                    if outcome > case.outcome {
                        case.outcome = outcome;
                        let attrs = attributes(body);
                        let text = if tag.ends_with('/') {
                            ""
                        } else {
                            &rest[..rest.find('<').unwrap_or(rest.len())]
                        };
                        let said =
                            attr(&attrs, "message").map_or_else(|| unescaped(text), str::to_owned);
                        case.message = first_line(&said);
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// Where the tag `rest` opens ends: its `>`, past any quoted value.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (at, c) in rest.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(at),
            _ => {}
        }
    }
    None
}

/// The case a `<testcase>` tag's attributes name.
fn case(body: &str) -> Case {
    let attrs = attributes(body);
    let name = attr(&attrs, "name").unwrap_or_default();
    let class = attr(&attrs, "class").or_else(|| attr(&attrs, "file"));
    Case {
        id: class.map_or_else(|| name.to_owned(), |class| format!("{class}::{name}")),
        outcome: Outcome::Passed,
        message: String::new(),
    }
}

/// A tag's attributes, each value decoded.
fn attributes(body: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut rest = body.split_once(char::is_whitespace).map_or("", |(_, r)| r);
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().to_owned();
        let value = rest[eq + 1..].trim_start();
        let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            break;
        };
        let Some(end) = value[1..].find(quote) else {
            break;
        };
        attrs.push((key, unescaped(&value[1..=end])));
        rest = &value[end + 2..];
    }
    attrs
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    (attrs.iter()).find_map(|(k, v)| (k == key).then_some(v.as_str()))
}

/// `text` with XML's entities and character references decoded; one it
/// does not know is left as it stands.
fn unescaped(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest.find(';').and_then(|end| {
            let c = match &rest[1..end] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                n => {
                    let n = n.strip_prefix('#')?;
                    let code = match n.strip_prefix('x') {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => n.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, end + 1))
        });
        let (c, len) = decoded.unwrap_or(('&', 1));
        out.push(c);
        rest = &rest[len..];
    }
    out + rest
}

/// The first line that says something, after any that name the test
/// alone, as `phpunit`'s failure text opens with the test's own name.
fn first_line(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let line = match lines.as_slice() {
        [named, said, ..] if !named.contains(' ') && named.contains("::") => said,
        [line, ..] => line,
        [] => "",
    };
    line.to_owned()
}
