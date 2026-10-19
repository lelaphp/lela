//! Whether a shell line can write one file (#1895): what `permit --after`
//! asks before it puts a `harness.self` file back, so that a person's edit
//! made while some session's call runs is not taken for the call's. The
//! answer is read from the line as [`argv`] lexes it, and only from what
//! the line can be seen to do to that file by its path:
//!
//! - a redirection that writes it (`>`, `>>`, `>|`, `&>`, `<>`);
//! - a writer with it among its operands: any program but the readers
//!   below, `sed` only with `-i`, and `cp`, `install`, `ln`, `rsync` only
//!   as their destination ([`Operands`]);
//! - an interpreter whose program names it: in a `-c` or `-e` word, in a
//!   file it runs (`python3 append.py`), or in a heredoc body handed to it
//!   (`python3 - <<EOF`);
//! - a program run by its path whose bytes name it (`./tool.sh`).
//!
//! A word is the file when, read where the call stands or where the line
//! runs a command, a `cd` or a git command's `-C` taking it there
//! ([`placed`]), it is that path ([`Place::is`]); a program names it
//! when the file's name stands in its text ([`names_file`]). What none of
//! that shows, a binary that holds the path where no word or file spells
//! it, the line cannot be read to write, and the sensor keeps the change.

use std::path::{Path, PathBuf};

use super::argv::{Argv, Line};
use super::rules::placed;

/// The file asked about, where the call stands: the sensor's side of the
/// question, which touches the disk so that this module does not.
pub(super) trait Place {
    /// Whether `word`, read where the call stands, is the file.
    fn is(&self, word: &str) -> bool;
    /// The file's name, which a program's text spells.
    fn name(&self) -> &str;
    /// The bytes of the file `word` names where the call stands, `None`
    /// for a word that names no file the sensor reads.
    fn read(&self, word: &str) -> Option<Vec<u8>>;
}

/// Which of a command's words it can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operands {
    /// None: a reader, or `sed` without `-i`.
    None,
    /// The last, the destination of a copy.
    Last,
    /// Any, the value after an option's `=` included.
    All,
}

/// Commands that read the files they name and write none of them, each
/// with the options that would make it write or run something; `git` by
/// the subcommands that leave the working tree alone.
const READERS: [(&str, &[&str]); 30] = [
    ("cat", &[]),
    ("head", &[]),
    ("tail", &[]),
    ("grep", &[]),
    ("egrep", &[]),
    ("fgrep", &[]),
    ("rg", &["--pre"]),
    ("wc", &[]),
    ("diff", &[]),
    ("cmp", &[]),
    ("stat", &[]),
    ("ls", &[]),
    ("file", &[]),
    ("nl", &[]),
    ("shasum", &[]),
    ("md5", &[]),
    ("md5sum", &[]),
    ("echo", &[]),
    ("printf", &[]),
    ("test", &[]),
    ("[", &[]),
    ("git diff", &["--output"]),
    ("git log", &["--output"]),
    ("git show", &["--output"]),
    ("git blame", &[]),
    ("git status", &[]),
    ("git grep", &[]),
    ("git ls-files", &[]),
    ("git add", &[]),
    ("git commit", &[]),
];

/// Commands whose last operand is the one they write.
const COPIES: [&str; 5] = ["cp", "install", "ln", "rsync", "scp"];

/// Programs that run a program text: from a word (`-c`, `-e`), a file, or
/// a heredoc on their stdin. A name beginning with `python` is one too.
const INTERPRETERS: [&str; 15] = [
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "node",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "awk",
    "gawk",
    "lua",
    "osascript",
];

/// Whether `line` can write the file at `place` (the module's list).
pub(super) fn can_write(line: &Line, place: &impl Place) -> bool {
    // Each directory a command of the line runs or reaches in, read from
    // where the call stands, as [`placed`] reads it.
    let placed = placed(Path::new(""), line).into_iter();
    let dirs: Vec<PathBuf> = placed.map(|(_, dir)| dir).collect();
    // The word as the shell would find it: where the call stands, and in
    // each of those.
    let spellings = |word: &str| -> Vec<String> {
        let mut all = vec![word.to_owned()];
        if !word.starts_with(['/', '~']) {
            all.extend(
                dirs.iter()
                    .map(|dir| dir.join(word).to_string_lossy().into_owned()),
            );
        }
        all
    };
    let is = |word: &str| spellings(word).iter().any(|w| place.is(w));
    let names = |text: &[u8]| names_file(text, place.name());
    let read_names = |word: &str| {
        spellings(word)
            .iter()
            .any(|w| place.read(w).is_some_and(|bytes| names(&bytes)))
    };
    let commands = line.commands();
    line.written().iter().any(|target| is(target))
        || commands.iter().any(|argv| {
            let operands = argv.words().get(1..).unwrap_or_default();
            let written = match operands_of(argv) {
                Operands::None => false,
                Operands::Last => operands.last().is_some_and(|w| is(w)),
                Operands::All => operands
                    .iter()
                    .any(|w| is(w) || w.split_once('=').is_some_and(|(_, value)| is(value))),
            };
            let program = interprets(argv)
                && operands
                    .iter()
                    .any(|w| names(w.as_bytes()) || read_names(w));
            let by_path = argv
                .words()
                .first()
                .is_some_and(|p| p.contains('/') && read_names(p));
            written || program || by_path
        })
        || line.bodies().iter().any(|body| {
            commands
                .get(body.commands.clone())
                .is_some_and(|line| line.iter().any(interprets))
                && names(body.text.as_bytes())
        })
}

/// Which of the command's words it can write.
fn operands_of(argv: &Argv) -> Operands {
    let words = argv.words();
    let carries = |options: &[&str]| {
        words
            .iter()
            .any(|w| options.iter().any(|option| w.starts_with(option)))
    };
    if READERS
        .iter()
        .any(|(phrase, writing)| argv.runs(phrase) && !carries(writing))
    {
        return Operands::None;
    }
    match argv.program() {
        "sed" if !words.iter().skip(1).any(|w| in_place(w)) => Operands::None,
        copy if COPIES.contains(&copy) => Operands::Last,
        _ => Operands::All,
    }
}

/// `sed`'s `-i`, `-i.bak`, `--in-place`, or a cluster carrying `i`.
fn in_place(word: &str) -> bool {
    word.starts_with("-i")
        || word.starts_with("--in-place")
        || word.strip_prefix('-').is_some_and(|cluster| {
            !cluster.starts_with('-')
                && cluster.contains('i')
                && cluster.chars().all(|c| c.is_ascii_alphabetic())
        })
}

/// Whether the command runs a program text ([`INTERPRETERS`]).
fn interprets(argv: &Argv) -> bool {
    let program = argv.program();
    INTERPRETERS.contains(&program) || program.starts_with("python")
}

/// Whether `text` names the file `name`: `name` standing in it with no
/// character of a file name on either side, so `docs/divergence.toml` and
/// `open('divergence.toml')` name `divergence.toml` and `divergence.toml.bak`
/// does not.
fn names_file(text: &[u8], name: &str) -> bool {
    let name = name.as_bytes();
    let part = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || b"._-".contains(b));
    !name.is_empty()
        && text.windows(name.len()).enumerate().any(|(at, window)| {
            window == name
                && !part(at.checked_sub(1).and_then(|i| text.get(i)))
                && !part(text.get(at + name.len()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file `path` relative to the call, a program file `append.py`
    /// that names `docs/divergence.toml`, and nothing else on disk.
    struct At(&'static str);

    impl Place for At {
        fn is(&self, word: &str) -> bool {
            word.strip_prefix("./").unwrap_or(word) == self.0
        }
        fn name(&self) -> &str {
            self.0.rsplit('/').next().unwrap_or(self.0)
        }
        fn read(&self, word: &str) -> Option<Vec<u8>> {
            (word == "append.py" || word == "./tool.sh")
                .then(|| b"open('docs/divergence.toml', 'a')".to_vec())
        }
    }

    fn can(line: &str, path: &'static str) -> bool {
        can_write(&Line::read(line), &At(path))
    }

    /// #1895: the routes by which a line writes the file.
    #[test]
    fn a_line_writes_what_it_redirects_into_or_hands_a_writer_or_an_interpreter() {
        assert!(can("echo x >> seams.toml", "seams.toml"));
        assert!(can(
            "cat <<'EOF' > docs/divergence.toml\nrow\nEOF",
            "docs/divergence.toml"
        ));
        assert!(can("cargo test &> oracle.toml", "oracle.toml"));
        assert!(can("cmd >& oracle.toml", "oracle.toml"));
        assert!(can("cmd <> oracle.toml", "oracle.toml"));
        assert!(can("bash -c 'echo x > seams.toml'", "seams.toml"));
        assert!(can("echo `echo x > seams.toml`", "seams.toml"));
        assert!(can(
            "cd docs && echo x >> divergence.toml",
            "docs/divergence.toml"
        ));
        // #2607: a `pushd`, a git `-C` and a `cd` to a variable the line
        // assigned move where a word is read, as a `cd` does.
        assert!(can(
            "pushd docs && echo x >> divergence.toml",
            "docs/divergence.toml"
        ));
        assert!(can(
            "git -C wt checkout -- docs/divergence.toml",
            "wt/docs/divergence.toml"
        ));
        assert!(can(
            "W=docs && cd \"$W\" && echo x >> divergence.toml",
            "docs/divergence.toml"
        ));
        assert!(can("sed -i '' -e 's/a/b/' seams.toml", "seams.toml"));
        assert!(can("sed -i.bak -e 's/a/b/' seams.toml", "seams.toml"));
        assert!(can("cp /tmp/m seams.toml", "seams.toml"));
        assert!(can("mv seams.toml /tmp/m", "seams.toml"));
        assert!(can("echo x | tee -a oracle.toml", "oracle.toml"));
        assert!(can("dd if=/tmp/m of=seams.toml", "seams.toml"));
        assert!(can("git checkout -- seams.toml", "seams.toml"));
        assert!(can("git diff --output=seams.toml", "seams.toml"));
        assert!(can(
            "python3 -c \"open('.claude/settings.json','a')\"",
            ".claude/settings.json"
        ));
        assert!(can(
            "python3 - <<'EOF'\nopen('seams.toml', 'a')\nEOF",
            "seams.toml"
        ));
        assert!(can(
            "bash <<'SH'\npython3 - <<'PY'\nopen('seams.toml', 'a')\nPY\nSH",
            "seams.toml"
        ));
        assert!(can("python3 append.py", "docs/divergence.toml"));
        assert!(can("./tool.sh", "docs/divergence.toml"));
    }

    /// What names the file, or another of its name, and cannot write it:
    /// the calls a person's edit must outlive.
    #[test]
    fn a_line_that_reads_quotes_or_mentions_the_file_does_not_write_it() {
        assert!(!can("grep -n api_items seams.toml", "seams.toml"));
        assert!(!can("cat seams.toml | head -5 > /tmp/m", "seams.toml"));
        assert!(!can(
            "git diff -- seams.toml && git -C x log -p seams.toml",
            "seams.toml"
        ));
        assert!(!can(
            "git add -- seams.toml && git commit -F /tmp/m -- seams.toml",
            "seams.toml"
        ));
        assert!(!can("cargo xtask status --section rules", "seams.toml"));
        assert!(!can("cmd < oracle.toml 2>&1 >&2", "oracle.toml"));
        assert!(!can("cp seams.toml /tmp/m", "seams.toml"));
        assert!(!can("sed -n 1,5p seams.toml", "seams.toml"));
        assert!(!can("echo x > /tmp/seams.toml", "seams.toml"));
        assert!(!can("echo x > myseams.toml", "seams.toml"));
        assert!(!can(
            "gh issue create --body 'seams.toml gains a row'",
            "seams.toml"
        ));
        assert!(!can(
            "cat > /tmp/m <<'EOF'\nseams.toml gains a row\nEOF",
            "seams.toml"
        ));
        assert!(!can(
            "git commit -F - <<'EOF'\nseams.toml: a row\nEOF",
            "seams.toml"
        ));
        assert!(!can("cat append.py", "docs/divergence.toml"));
        assert!(!can("computed run AGENTS.md", "seams.toml"));
    }
}
