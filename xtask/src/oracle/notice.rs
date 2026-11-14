//! `NOTICE`'s php-src tag held to the pin (ruled on #4663): the sentence
//! that says the derived parts follow php-src "at tag" names the tag and
//! its commit, `<tag> (<commit>)`, a second statement of `oracle.toml`'s
//! `[source]`. `oracle propose` writes it from the checkout a bump moves
//! to, and the tracked test here holds it to the pin, so a pin moved
//! without it fails at the commit that moves it.

use std::ops::Range;

/// `NOTICE`'s one sentence naming the tag php-src's derived parts follow:
/// the words `at tag`, a blank or a line break, the tag, and its commit in
/// parentheses, where the text names them.
pub(super) struct Sentence<'a> {
    text: &'a str,
    tag: Range<usize>,
    commit: Range<usize>,
}

impl<'a> Sentence<'a> {
    /// The one sentence of `text`; otherwise that it has none, or how many.
    pub(super) fn of(text: &'a str) -> Result<Sentence<'a>, String> {
        let pattern = regex_lite::Regex::new(r"\bat tag\s+(\S+) \(([0-9a-f]+)\)")
            .map_err(|e| e.to_string())?;
        let found: Vec<(Range<usize>, Range<usize>)> = (pattern.captures_iter(text))
            .filter_map(|named| Some((named.get(1)?.range(), named.get(2)?.range())))
            .collect();
        let [(tag, commit)] = &found[..] else {
            return Err(if found.is_empty() {
                "names php-src at no tag: the sentence `at tag <tag> (<commit>)` is missing"
                    .to_owned()
            } else {
                format!(
                    "names php-src `at tag <tag> (<commit>)` in {} places, where its one sentence names it once",
                    found.len()
                )
            });
        };
        Ok(Sentence {
            text,
            tag: tag.clone(),
            commit: commit.clone(),
        })
    }

    /// The tag the sentence names, which the hold reads.
    #[cfg(test)]
    fn tag(&self) -> &'a str {
        &self.text[self.tag.clone()]
    }

    /// The commit the sentence names, which the hold reads.
    #[cfg(test)]
    fn commit(&self) -> &'a str {
        &self.text[self.commit.clone()]
    }

    /// The text with the sentence naming `tag` and `commit`, and nothing
    /// else moved: no line is wrapped again, so a line a longer tag leaves
    /// past the paragraph's wrap is a person's to reflow.
    pub(super) fn naming(&self, tag: &str, commit: &str) -> String {
        let text = self.text;
        [
            &text[..self.tag.start],
            tag,
            &text[self.tag.end..self.commit.start],
            commit,
            &text[self.commit.end..],
        ]
        .concat()
    }
}

/// The `NOTICE` at `path` names, in its one sentence, the pinned `tag` and
/// `commit`; otherwise what it names, or that it names none or several.
#[cfg(test)]
fn holds(path: &std::path::Path, tag: &str, commit: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let named = Sentence::of(&text).map_err(|why| format!("{} {why}", path.display()))?;
    if (named.tag(), named.commit()) == (tag, commit) {
        return Ok(());
    }
    Err(format!(
        "{} names php-src at {} ({}), and oracle.toml pins {tag} ({commit}): the tag moves with the oracle, and `cargo xtask oracle propose` writes it (#4663)",
        path.display(),
        named.tag(),
        named.commit()
    ))
}

#[cfg(test)]
mod tests {
    use super::{Sentence, holds};

    /// The fixture's `NOTICE` names the tag the draft named, php-8.5.9,
    /// which the hold holds to a pin of that tag and commit and fails
    /// against any other, naming both. No tag here is the pin's, which no
    /// source file states.
    #[test]
    fn a_notice_naming_another_tag_fails_naming_both() {
        let fixture = crate::package_dir().join("fixtures/oracle/notice-drift/NOTICE");
        let why = holds(&fixture, "php-9.0.0", "0123abcd").unwrap_err();
        assert!(
            why.contains(
                "names php-src at php-8.5.9 (dd6e76cc), and oracle.toml pins php-9.0.0 (0123abcd)"
            ),
            "{why}"
        );
        assert!(holds(&fixture, "php-8.5.9", "0123abcd").is_err());
        assert_eq!(holds(&fixture, "php-8.5.9", "dd6e76cc"), Ok(()));
    }

    /// The sentence is found across the line break the paragraph puts in
    /// it, and is refused where it is missing or stands twice, the message
    /// saying which. Naming another tag moves the tag and the commit and
    /// nothing else, the line break kept.
    #[test]
    fn the_sentence_is_found_once_and_renamed_where_it_stands() {
        let text = "derived from php-src at tag\nphp-1.2.3 (abcdef01): data\n";
        let named = Sentence::of(text).unwrap();
        assert_eq!((named.tag(), named.commit()), ("php-1.2.3", "abcdef01"));
        assert_eq!(
            named.naming("php-1.2.10", "12345678"),
            "derived from php-src at tag\nphp-1.2.10 (12345678): data\n"
        );
        let missing = Sentence::of("derived from php-src\n").err().unwrap();
        assert!(missing.contains("is missing"), "{missing}");
        let twice = Sentence::of(&text.repeat(2)).err().unwrap();
        assert!(twice.contains("in 2 places"), "{twice}");
    }

    /// The tracked `NOTICE` names the tag and the commit `oracle.toml`'s
    /// `[source]` pins.
    #[cfg(tracked)]
    #[test]
    fn the_notice_names_the_pinned_php_src_tag() {
        let root = crate::package_dir().parent().unwrap().to_path_buf();
        let manifest = crate::manifest::Manifest::load(&root).unwrap();
        let source = &manifest.pin().unwrap().source;
        assert_eq!(
            holds(&root.join("NOTICE"), &source.tag, &source.commit),
            Ok(())
        );
    }
}
