//! The glossary, `CONTEXT.md`, read for its `_Avoid_` lists (harness-design.md
//! §5). A term is a `**Name**:` line; its avoided phrases are the entries,
//! separated by commas or semicolons, of the `_Avoid_:` line that follows.
//! An entry may carry an explanation the sensor does not match on: a
//! parenthetical, a qualifier (`param as nouns for the value`, `` `Ret` for
//! a return ``, `class table alone`), backticks or quotes. What remains is
//! the phrase.
//!
//! Matching is by whole word and ignores case, so `Registry` the type is
//! `registry` the word. The one scoping the design asks for, `Result` is not
//! `result`, is read from the glossary itself: an identifier the entry's
//! parenthetical writes in backticks (`result (which means `Result` in
//! Rust)`) is a spelling that is code, and that spelling, written exactly,
//! is not a hit. Words are runs of letters and digits; a hyphen or an
//! underscore separates two words, and a possessive `'s` is dropped.

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
/// explanation; its words, lowercased; and the spellings its explanation
/// marks as code, which are not the word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    pub text: String,
    pub words: Vec<String>,
    pub code: Vec<String>,
}

impl Phrase {
    /// Whether `words` (as written, any case) carry this phrase, starting
    /// at some position: the lowercased words match, and a one-word phrase
    /// is not met by a spelling its glossary entry marks as code.
    pub fn occurs_in(&self, words: &[String]) -> bool {
        words.windows(self.words.len()).any(|window| {
            window
                .iter()
                .zip(&self.words)
                .all(|(w, p)| w.to_lowercase() == *p)
                && !(self.words.len() == 1 && self.code.contains(&window[0]))
        })
    }
}

/// One avoided phrase with every term that avoids it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avoided {
    pub phrase: Phrase,
    pub terms: Vec<String>,
}

impl Glossary {
    /// Read the glossary file.
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

    /// The term named `name`, if the glossary has it.
    #[cfg(test)]
    pub fn term(&self, name: &str) -> Option<&Term> {
        self.terms.iter().find(|t| t.name == name)
    }

    /// Every avoided phrase, once, with the terms that avoid it, in the
    /// order the glossary first states each.
    pub fn avoided_phrases(&self) -> Vec<Avoided> {
        let mut out: Vec<Avoided> = Vec::new();
        for term in &self.terms {
            for phrase in &term.avoid {
                match out.iter_mut().find(|a| a.phrase.words == phrase.words) {
                    Some(seen) => seen.terms.push(term.name.clone()),
                    None => out.push(Avoided {
                        phrase: phrase.clone(),
                        terms: vec![term.name.clone()],
                    }),
                }
            }
        }
        out
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
/// cut at the first qualifier; the identifiers the parentheticals write in
/// backticks kept as its code spellings. `None` for an entry that leaves
/// no words.
fn phrase(entry: &str) -> Option<Phrase> {
    let mut text = String::new();
    let mut code = Vec::new();
    let mut depth = 0u32;
    let mut span: Option<String> = None;
    for c in entry.chars() {
        match c {
            '`' if depth > 0 => match span.take() {
                Some(s) if is_identifier(&s) => code.push(s),
                Some(_) => {}
                None => span = Some(String::new()),
            },
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '`' | '"' | '\u{201c}' | '\u{201d}' => {}
            _ if depth == 0 => text.push(c),
            _ => {
                if let Some(s) = span.as_mut() {
                    s.push(c);
                }
            }
        }
    }
    let cut = QUALIFIERS
        .iter()
        .filter_map(|q| text.find(q))
        .min()
        .unwrap_or(text.len());
    let text = text[..cut].trim().to_owned();
    let words = words(&text)
        .iter()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>();
    (!words.is_empty()).then_some(Phrase { text, words, code })
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// The words of a run of prose or of a glossary entry, as written: runs of
/// letters and digits, a possessive `'s` dropped.
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
    fn a_backticked_identifier_in_the_explanation_is_a_code_spelling() {
        let p = phrase("result (which means `Result` in Rust)").unwrap();
        assert_eq!(p.words, ["result"]);
        assert_eq!(p.code, ["Result"]);
        assert!(p.occurs_in(&["the".into(), "result".into()]));
        assert!(p.occurs_in(&["RESULT".into()]));
        assert!(!p.occurs_in(&["Result".into()]));
        assert!(!p.occurs_in(&["results".into()]));

        // A code span that is not an identifier marks nothing.
        let p = phrase("feedback (the `MapPtr<[Feedback]>` of §5.14)").unwrap();
        assert!(p.code.is_empty());
        assert!(p.occurs_in(&["Feedback".into()]));
    }

    #[test]
    fn phrases_match_any_case_and_several_words() {
        let p = phrase("plugin list").unwrap();
        assert!(p.occurs_in(&["Plugin".into(), "List".into()]));
        assert!(!p.occurs_in(&["plugin".into(), "lists".into()]));
        let p = phrase("AST as a distinct thing").unwrap();
        assert!(p.occurs_in(&["ast".into()]));
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
    /// The tracked glossary, read as the sensor reads it.
    #[test]
    fn the_glossary_reads_each_term_with_its_avoided_phrases() {
        let glossary = Glossary::load(&crate::built_root().join("CONTEXT.md")).unwrap();
        let serve = glossary.term("Serve").unwrap();
        let avoided: Vec<&str> = serve.avoid.iter().map(|a| a.text.as_str()).collect();
        assert_eq!(avoided, ["run", "execute", "handle"]);

        // A parenthetical explanation and a qualifier are not part of the phrase.
        let linked = glossary.term("Linked table").unwrap();
        let avoided: Vec<&str> = linked.avoid.iter().map(|a| a.text.as_str()).collect();
        assert_eq!(avoided, ["class table", "registry"]);
        let native = glossary.term("Parameter / Argument").unwrap();
        let avoided: Vec<&str> = native.avoid.iter().map(|a| a.text.as_str()).collect();
        assert_eq!(avoided, ["arg", "param", "Ret"]);

        // A phrase avoided under two terms names both.
        let phrases = glossary.avoided_phrases();
        let registry = phrases
            .iter()
            .find(|a| a.phrase.text == "registry")
            .unwrap();
        assert_eq!(registry.terms, ["Linked table", "Bundle"]);

        // The glossary's own carve-out: `result (which means `Result` in Rust)`.
        let result = phrases.iter().find(|a| a.phrase.text == "result").unwrap();
        assert_eq!(result.phrase.code, ["Result"]);
    }
}
