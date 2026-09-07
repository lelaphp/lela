//! The glossary, `CONTEXT.md`, read for its `_Avoid_` lists (harness-design.md
//! §5). A term is a `**Name**:` line; its avoided phrases are the
//! comma-separated entries of the `_Avoid_:` line that follows. An entry may
//! carry an explanation the sensor does not match on: a parenthetical, a
//! qualifier (`param as nouns for the value`, `` `Ret` for a return ``,
//! `class table alone`), backticks or quotes. What remains is the phrase.
//!
//! Matching is case-sensitive and by whole word: the glossary avoids
//! `result` and `Ret` as written, so `Result` the type is not `result` the
//! word. Words are runs of letters and digits; a hyphen or an underscore
//! separates two words, and a possessive `'s` is dropped.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The glossary's terms, in file order.
#[derive(Debug, Clone, Default)]
pub struct Glossary {
    pub terms: Vec<Term>,
}

/// One `**Name**:` entry and the phrases it avoids.
#[derive(Debug, Clone)]
pub struct Term {
    pub name: String,
    pub avoid: Vec<Phrase>,
}

/// One avoided phrase: its text as the glossary wrote it, less the
/// explanation, and its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    pub text: String,
    pub words: Vec<String>,
}

/// One avoided phrase with every term that avoids it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avoided {
    pub text: String,
    pub words: Vec<String>,
    pub terms: Vec<String>,
}

impl Glossary {
    pub fn load(path: &Path) -> Result<Glossary> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Glossary::parse(&text).with_context(|| format!("{}", path.display()))
    }

    /// Read the terms out of the glossary text. An `_Avoid_` line with no
    /// term above it is refused; a term with no `_Avoid_` line avoids nothing.
    pub fn parse(text: &str) -> Result<Glossary> {
        let mut terms: Vec<Term> = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if let Some(name) = line
                .strip_prefix("**")
                .and_then(|rest| rest.strip_suffix("**:"))
            {
                terms.push(Term {
                    name: name.trim().to_owned(),
                    avoid: Vec::new(),
                });
            } else if let Some(list) = line.strip_prefix("_Avoid_:") {
                let Some(term) = terms.last_mut() else {
                    bail!("line {}: an _Avoid_ list with no term above it", index + 1);
                };
                if !term.avoid.is_empty() {
                    bail!(
                        "line {}: a second _Avoid_ list under \"{}\"",
                        index + 1,
                        term.name
                    );
                }
                term.avoid = split_entries(list)
                    .iter()
                    .filter_map(|e| phrase(e))
                    .collect();
            }
        }
        Ok(Glossary { terms })
    }

    pub fn term(&self, name: &str) -> Option<&Term> {
        self.terms.iter().find(|t| t.name == name)
    }

    /// Every avoided phrase, once, with the terms that avoid it, in the
    /// order the glossary first states each.
    pub fn avoided_phrases(&self) -> Vec<Avoided> {
        let mut out: Vec<Avoided> = Vec::new();
        for term in &self.terms {
            for phrase in &term.avoid {
                match out.iter_mut().find(|a| a.words == phrase.words) {
                    Some(seen) => seen.terms.push(term.name.clone()),
                    None => out.push(Avoided {
                        text: phrase.text.clone(),
                        words: phrase.words.clone(),
                        terms: vec![term.name.clone()],
                    }),
                }
            }
        }
        out
    }

    /// The avoided phrase written as `text`, if the glossary avoids it.
    pub fn avoided(&self, text: &str) -> Option<Avoided> {
        let words = words(text);
        self.avoided_phrases()
            .into_iter()
            .find(|a| a.words == words)
    }
}

/// The entries of one `_Avoid_` line: split on commas and semicolons
/// outside parentheses.
fn split_entries(list: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut depth = 0u32;
    let mut current = String::new();
    for c in list.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' | ';' if depth == 0 => {
                entries.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    entries.push(current);
    entries
        .into_iter()
        .map(|e| e.trim().to_owned())
        .filter(|e| !e.is_empty())
        .collect()
}

/// The qualifiers an entry may carry after its phrase; the phrase ends at
/// the first.
const QUALIFIERS: [&str; 4] = [" as ", " for ", " alone", " which "];

/// The phrase of one entry: parentheticals, backticks and quotes removed,
/// cut at the first qualifier. `None` for an entry that leaves no words.
fn phrase(entry: &str) -> Option<Phrase> {
    let mut text = String::new();
    let mut depth = 0u32;
    for c in entry.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '`' | '"' | '\u{201c}' | '\u{201d}' => {}
            _ if depth == 0 => text.push(c),
            _ => {}
        }
    }
    let cut = QUALIFIERS
        .iter()
        .filter_map(|q| text.find(q))
        .min()
        .unwrap_or(text.len());
    let text = text[..cut].trim().to_owned();
    let words = words(&text);
    (!words.is_empty()).then_some(Phrase { text, words })
}

/// The words of a run of prose or of a glossary entry: runs of letters and
/// digits, a possessive `'s` dropped.
pub fn words(text: &str) -> Vec<String> {
    let text = text.replace("'s", "").replace("\u{2019}s", "");
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Glossary, phrase, words};

    #[test]
    fn entries_lose_their_explanations() {
        let p = |e: &str| phrase(e).map(|p| p.text);
        assert_eq!(p("`Func` table"), Some("Func table".into()));
        assert_eq!(p("`Ret` for a return"), Some("Ret".into()));
        assert_eq!(p("param as nouns for the value"), Some("param".into()));
        assert_eq!(
            p("class table alone (functions too)"),
            Some("class table".into())
        );
        assert_eq!(p("globals (php's word)"), Some("globals".into()));
        assert_eq!(
            p("\"no return-type info\" (Mahout's)"),
            Some("no return-type info".into())
        );
        assert_eq!(p("`api-check`'s tables"), Some("api-check's tables".into()));
        assert_eq!(p("(nothing)"), None);
    }

    #[test]
    fn words_split_on_hyphens_and_drop_possessives() {
        assert_eq!(
            words("no return-type info"),
            ["no", "return", "type", "info"]
        );
        assert_eq!(words("api-check's tables"), ["api", "check", "tables"]);
        assert_eq!(words("vm_interrupt"), ["vm", "interrupt"]);
    }

    #[test]
    fn a_term_with_no_avoid_line_avoids_nothing() {
        let g = Glossary::parse("**A**:\nsome text\n\n**B**:\n_Avoid_: x, y z\n").unwrap();
        assert!(g.term("A").unwrap().avoid.is_empty());
        let b: Vec<_> = g
            .term("B")
            .unwrap()
            .avoid
            .iter()
            .map(|p| p.text.as_str())
            .collect();
        assert_eq!(b, ["x", "y z"]);
    }

    #[test]
    fn an_avoid_line_with_no_term_is_refused() {
        assert!(Glossary::parse("_Avoid_: x\n").is_err());
    }
}
