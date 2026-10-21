//! One `.phpt` file and its sections, read as upstream's runner reads them
//! at the pinned tag (`TestFile`, run-tests.php:3672-3869): the first line
//! is `--TEST--`, a line matching `^--([_A-Z]+)--` opens a section wherever
//! it stands, a `FILE` section ends at `===DONE===`, a section outside the
//! allowed list or a second non-empty one borks the test, and the three
//! `_EXTERNAL` forms are loaded from beside the test. The reasons are
//! upstream's text, so a borked test reads the same here as there.
//!
//! A section is bytes, not text. Part of the corpus is written in ISO
//! 8859-1 or in bytes that are no encoding at all (`ext/pcre/tests/
//! locales.phpt`), and a program whose bytes are not the file's is not the
//! test. The sections a runner reads as words are converted where they are
//! read, and never on the way in.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use regex_lite::Regex;

/// The sections upstream allows (run-tests.php:3678-3687). `TEST` is not
/// among them: it is consumed before the loop, so a second one borks.
pub const ALLOWED: [&str; 35] = [
    "EXPECT",
    "EXPECTF",
    "EXPECTREGEX",
    "EXPECTREGEX_EXTERNAL",
    "EXPECT_EXTERNAL",
    "EXPECTF_EXTERNAL",
    "EXPECTHEADERS",
    "POST",
    "POST_RAW",
    "GZIP_POST",
    "DEFLATE_POST",
    "PUT",
    "GET",
    "COOKIE",
    "ARGS",
    "FILE",
    "FILEEOF",
    "FILE_EXTERNAL",
    "REDIRECTTEST",
    "CAPTURE_STDIO",
    "STDIN",
    "CGI",
    "PHPDBG",
    "INI",
    "ENV",
    "EXTENSIONS",
    "SKIPIF",
    "XFAIL",
    "XLEAK",
    "CLEAN",
    "CREDITS",
    "DESCRIPTION",
    "CONFLICTS",
    "WHITESPACE_SENSITIVE",
    "FLAKY",
];

/// Why a test cannot be run at all: upstream's `BorkageException` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Borked(pub String);

impl std::fmt::Display for Borked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One `.phpt` file, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phpt {
    pub path: PathBuf,
    pub sections: BTreeMap<String, Vec<u8>>,
}

impl Phpt {
    /// Read and parse the file at `path`; an unreadable file borks, as an
    /// empty one does upstream.
    pub fn load(path: &Path) -> Result<Phpt, Borked> {
        let bytes = fs::read(path)
            .map_err(|e| Borked(format!("cannot read test [{}]: {e}", path.display())))?;
        Phpt::parse(path, &bytes)
    }

    /// Parse `bytes` as the file at `path`, from which the `_EXTERNAL`
    /// sections are resolved.
    pub fn parse(path: &Path, bytes: &[u8]) -> Result<Phpt, Borked> {
        let shown = path.display();
        let mut lines = bytes.split_inclusive(|b| *b == b'\n');
        let first = lines
            .next()
            .ok_or_else(|| Borked(format!("empty test [{shown}]")))?;
        if !first.starts_with(b"--TEST--") {
            return Err(Borked(format!("tests must start with --TEST-- [{shown}]")));
        }
        let mut sections: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        sections.insert("TEST".to_owned(), Vec::new());
        let mut section = "TEST".to_owned();
        let mut in_file = false;
        let mut done = false;
        for line in lines {
            if let Some(name) = opener(line) {
                if sections.get(&name).is_some_and(|body| !body.is_empty()) {
                    return Err(Borked(format!("duplicated {name} section")));
                }
                if !ALLOWED.contains(&name.as_str()) {
                    return Err(Borked(format!("Unknown section \"{name}\"")));
                }
                in_file = matches!(name.as_str(), "FILE" | "FILEEOF" | "FILE_EXTERNAL");
                done = false;
                section = name;
                sections.insert(section.clone(), Vec::new());
                continue;
            }
            if !done {
                sections
                    .entry(section.clone())
                    .or_default()
                    .extend_from_slice(line);
            }
            if in_file && line.trim_ascii_end() == b"===DONE===" {
                done = true;
            }
        }
        let mut phpt = Phpt {
            path: path.to_path_buf(),
            sections,
        };
        phpt.settle()?;
        Ok(phpt)
    }

    /// Upstream's `validateAndProcess`: `FILEEOF` becomes `FILE`, each
    /// `_EXTERNAL` section is loaded from beside the test, and exactly one
    /// program section and one expectation are required.
    fn settle(&mut self) -> Result<(), Borked> {
        if self.has("REDIRECTTEST") {
            return Ok(());
        }
        let programs = ["FILE", "FILEEOF", "FILE_EXTERNAL"]
            .iter()
            .filter(|s| self.has(s))
            .count();
        if !self.has("PHPDBG") && programs != 1 {
            return Err(Borked("missing section --FILE--".to_owned()));
        }
        if let Some(body) = self.sections.remove("FILEEOF") {
            let kept = body.len()
                - body
                    .iter()
                    .rev()
                    .take_while(|b| matches!(b, b'\r' | b'\n'))
                    .count();
            self.sections
                .insert("FILE".to_owned(), body[..kept].to_vec());
        }
        for prefix in ["FILE", "EXPECT", "EXPECTF", "EXPECTREGEX"] {
            let key = format!("{prefix}_EXTERNAL");
            let Some(named) = self.text(&key) else {
                continue;
            };
            let dir = self
                .path
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
            let file = dir.join(named.replace("..", "").trim());
            let body = fs::read(&file).map_err(|_| {
                Borked(format!(
                    "could not load --{key}-- {}/{}",
                    dir.display(),
                    file.display()
                ))
            })?;
            self.sections.insert(prefix.to_owned(), body);
        }
        let expectations = ["EXPECT", "EXPECTF", "EXPECTREGEX"]
            .iter()
            .filter(|s| self.has(s))
            .count();
        if expectations != 1 {
            return Err(Borked(
                "missing section --EXPECT--, --EXPECTF-- or --EXPECTREGEX--".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn has(&self, name: &str) -> bool {
        self.sections.contains_key(name)
    }

    /// The section's bytes, which is what a program and an expectation are.
    pub fn section(&self, name: &str) -> Option<&[u8]> {
        self.sections.get(name).map(Vec::as_slice)
    }

    /// The section as words, for the sections a runner reads rather than
    /// runs: `--INI--`, `--ENV--`, `--EXTENSIONS--` and their like. A byte
    /// that is no character is the replacement character, which in a
    /// section of words is already a broken test.
    pub fn text(&self, name: &str) -> Option<Cow<'_, str>> {
        self.section(name).map(String::from_utf8_lossy)
    }

    /// The section when it is present and not empty, which is what
    /// upstream asks before it runs `SKIPIF`.
    pub fn filled(&self, name: &str) -> Option<&[u8]> {
        self.section(name).filter(|body| !body.is_empty())
    }

    /// Whether the test is a CGI one, which this runner does not host
    /// (upstream's `isCGI`, run-tests.php:3737-3748).
    pub fn is_cgi(&self) -> bool {
        self.has("CGI")
            || [
                "GET",
                "POST",
                "GZIP_POST",
                "DEFLATE_POST",
                "POST_RAW",
                "PUT",
                "COOKIE",
                "EXPECTHEADERS",
            ]
            .iter()
            .any(|s| self.filled(s).is_some())
    }
}

/// The conflict keys of the test at `path` as upstream reads them
/// (run-tests.php:1344-1363): its `--CONFLICTS--` section, found in the
/// file's text by upstream's own pattern and not by the parser above,
/// else its directory's `CONFLICTS` file, read once per directory into
/// `dirs`, and none where the directory has none.
pub fn conflicts(path: &Path, dirs: &mut BTreeMap<PathBuf, Vec<String>>) -> Vec<String> {
    let text = String::from_utf8_lossy(&fs::read(path).unwrap_or_default()).into_owned();
    let section = Regex::new(r"(?ms)^--CONFLICTS--(.+?)^--").ok();
    if let Some(found) = section.as_ref().and_then(|pattern| pattern.captures(&text)) {
        return keys(&found[1]);
    }
    let dir = path.parent().unwrap_or(path).to_path_buf();
    dirs.entry(dir)
        .or_insert_with_key(|dir| {
            fs::read_to_string(dir.join("CONFLICTS"))
                .map_or_else(|_| Vec::new(), |text| keys(&text))
        })
        .clone()
}

/// Upstream's `parse_conflicts` (run-tests.php:3227-3232): `#` to the end
/// of its line dropped, the text trimmed, and one key a line, trimmed.
fn keys(text: &str) -> Vec<String> {
    let bare = Regex::new("#.*").map_or_else(
        |_| text.to_owned(),
        |comment| comment.replace_all(text, "").into_owned(),
    );
    bare.trim_matches(super::adapter::TRIM)
        .split('\n')
        .map(|key| key.trim_matches(super::adapter::TRIM).to_owned())
        .collect()
}

/// The section a line opens, if it opens one: upstream's
/// `preg_match('/^--([_A-Z]+)--/')`, which is not anchored at the end.
fn opener(line: &[u8]) -> Option<String> {
    let rest = line.strip_prefix(b"--")?;
    let width = rest
        .iter()
        .take_while(|b| b.is_ascii_uppercase() || **b == b'_')
        .count();
    if width == 0 || !rest[width..].starts_with(b"--") {
        return None;
    }
    String::from_utf8(rest[..width].to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Phpt, Borked> {
        Phpt::parse(Path::new("/corpus/x.phpt"), text.as_bytes())
    }

    fn section<'a>(phpt: &'a Phpt, name: &str) -> Option<&'a str> {
        phpt.section(name)
            .map(|body| std::str::from_utf8(body).unwrap())
    }

    #[test]
    fn a_line_opens_a_section_wherever_it_stands() {
        assert_eq!(opener(b"--FILE--\n").as_deref(), Some("FILE"));
        assert_eq!(
            opener(b"--POST_RAW-- trailing\n").as_deref(),
            Some("POST_RAW")
        );
        assert_eq!(opener(b"-- FILE --\n"), None);
        assert_eq!(opener(b"--file--\n"), None);
        assert_eq!(opener(b"echo 1;\n"), None);
    }

    #[test]
    fn the_sections_are_the_text_between_the_openers() {
        let phpt = parse("--TEST--\nname\n--FILE--\n<?php echo 1;\n--EXPECT--\n1\n").unwrap();
        assert_eq!(phpt.text("TEST").unwrap().trim(), "name");
        assert_eq!(section(&phpt, "FILE"), Some("<?php echo 1;\n"));
        assert_eq!(section(&phpt, "EXPECT"), Some("1\n"));
    }

    #[test]
    fn a_section_is_bytes_and_not_every_test_is_utf_8() {
        let mut text = b"--TEST--\nn\n--FILE--\n<?php echo \"".to_vec();
        text.extend_from_slice(&[0xe1, 0xe9, 0xed]);
        text.extend_from_slice(b"\";\n--EXPECT--\n\xe1\xe9\xed\n");
        let phpt = Phpt::parse(Path::new("/corpus/latin.phpt"), &text).unwrap();
        assert_eq!(phpt.section("EXPECT"), Some(&[0xe1, 0xe9, 0xed, b'\n'][..]));
        assert!(
            phpt.section("FILE")
                .is_some_and(|body| body.ends_with(b"\xe1\xe9\xed\";\n")),
            "the program is the file's bytes"
        );
    }

    #[test]
    fn a_file_section_ends_at_done() {
        let phpt =
            parse("--TEST--\nn\n--FILE--\n<?php echo 1;\n===DONE===\nafter\n--EXPECT--\n1\n")
                .unwrap();
        assert_eq!(section(&phpt, "FILE"), Some("<?php echo 1;\n===DONE===\n"));
    }

    #[test]
    fn fileeof_loses_its_trailing_newlines_and_becomes_the_program() {
        let phpt = parse("--TEST--\nn\n--FILEEOF--\n<?php echo 1;\n\n--EXPECT--\n1\n").unwrap();
        assert_eq!(section(&phpt, "FILE"), Some("<?php echo 1;"));
        assert!(!phpt.has("FILEEOF"));
    }

    #[test]
    fn the_borkages_are_upstreams() {
        assert_eq!(
            parse("nope\n").unwrap_err(),
            Borked("tests must start with --TEST-- [/corpus/x.phpt]".to_owned())
        );
        assert_eq!(
            parse("").unwrap_err(),
            Borked("empty test [/corpus/x.phpt]".to_owned())
        );
        assert_eq!(
            parse("--TEST--\nn\n--NOPE--\nx\n").unwrap_err(),
            Borked("Unknown section \"NOPE\"".to_owned())
        );
        assert_eq!(
            parse("--TEST--\nn\n--FILE--\nx\n--FILE--\ny\n").unwrap_err(),
            Borked("duplicated FILE section".to_owned())
        );
        assert_eq!(
            parse("--TEST--\nn\n--EXPECT--\n1\n").unwrap_err(),
            Borked("missing section --FILE--".to_owned())
        );
        assert_eq!(
            parse("--TEST--\nn\n--FILE--\nx\n").unwrap_err(),
            Borked("missing section --EXPECT--, --EXPECTF-- or --EXPECTREGEX--".to_owned())
        );
        assert_eq!(
            parse("--TEST--\nn\n--FILE--\nx\n--EXPECT--\n1\n--EXPECTF--\n%s\n").unwrap_err(),
            Borked("missing section --EXPECT--, --EXPECTF-- or --EXPECTREGEX--".to_owned())
        );
    }

    #[test]
    fn a_cgi_shaped_test_says_so() {
        let cgi = parse("--TEST--\nn\n--CGI--\n--FILE--\nx\n--EXPECT--\n1\n").unwrap();
        assert!(cgi.is_cgi());
        let post = parse("--TEST--\nn\n--POST--\na=1\n--FILE--\nx\n--EXPECT--\n1\n").unwrap();
        assert!(post.is_cgi());
        let cli = parse("--TEST--\nn\n--FILE--\nx\n--EXPECT--\n1\n").unwrap();
        assert!(!cli.is_cgi());
    }

    /// The keys of the fixture at `xtask/fixtures/phpt/conflicts`: a
    /// test's own section, a comment in it dropped; none where neither the
    /// test nor its directory names one; the directory's `CONFLICTS` file,
    /// read once however many of its tests ask; and a test's own section
    /// standing in place of its directory's file.
    #[test]
    fn the_keys_are_the_sections_else_the_directorys_file() {
        let fixture = crate::package_dir().join("fixtures/phpt/conflicts");
        let mut dirs = BTreeMap::new();
        let mut keys = |test: &str| conflicts(&fixture.join(test), &mut dirs);
        assert_eq!(keys("a-keyed/one.phpt"), ["shared"]);
        assert_eq!(keys("a-keyed/two.phpt"), ["shared"]);
        assert_eq!(keys("a-keyed/free1.phpt"), Vec::<String>::new());
        assert_eq!(keys("b-alone/all.phpt"), ["all"]);
        assert_eq!(keys("c-directory/one.phpt"), ["directory"]);
        assert_eq!(keys("c-directory/two.phpt"), ["directory"]);
        assert_eq!(keys("c-directory/own.phpt"), ["own"]);
        assert_eq!(dirs.len(), 2, "{dirs:?}");
    }

    /// Upstream's `parse_conflicts` to the letter: an empty line between
    /// two keys is a key of its own, and so is a text of comments alone.
    #[test]
    fn the_keys_are_split_and_trimmed_as_upstream_splits_them() {
        assert_eq!(keys(" a # why\n\n\tb \r\n"), ["a", "", "b"]);
        assert_eq!(keys("# nothing\n"), [""]);
    }
}
