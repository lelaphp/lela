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
//!
//! Whether a command can change the checkout it runs in at all is the
//! other question asked here ([`changes`], #3621): what a rule with
//! `writes = true` fires on, so that one session's writes in a checkout
//! another owns are refused and its reads are not.

use std::path::{Path, PathBuf};

use super::argv::{Argv, Line};
use super::rules::{Places, placed};

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

/// Commands that leave the checkout they run in as it was, beside
/// [`READERS`]: what writes nothing of its working tree, its index, its
/// HEAD or its operation files, which is all the owner lock covers
/// (#3755). A change of directory; what reads the repository's history,
/// refs or settings; what writes only what checkouts share, refs, objects,
/// settings and a new worktree beside them; what reaches the tracker alone;
/// and the instruments that say where things stand. Each with the words
/// that would make it write.
const LEAVES: [(&str, &[&str]); 38] = [
    ("cd", &[]),
    ("pushd", &[]),
    ("popd", &[]),
    ("pwd", &[]),
    ("true", &[]),
    ("false", &[]),
    ("sleep", &[]),
    ("date", &[]),
    ("which", &[]),
    ("basename", &[]),
    ("dirname", &[]),
    ("realpath", &[]),
    ("cut", &[]),
    ("tr", &[]),
    ("jq", &[]),
    ("sort", &["-o", "--output"]),
    // `-exec` stands for `-execdir` too, `-ok` for `-okdir`, `-fprint` for
    // `-fprint0` and `-fprintf` (#4162).
    ("find", &["-delete", "-exec", "-ok", "-fprint", "-fls"]),
    ("gh", &["checkout", "clone"]),
    ("git rev-parse", &[]),
    ("git rev-list", &[]),
    ("git merge-base", &[]),
    ("git ls-tree", &[]),
    ("git cat-file", &[]),
    ("git describe", &[]),
    ("git for-each-ref", &[]),
    ("git fetch", &[]),
    // It writes `refs/remotes/<name>/HEAD` alone (#4066).
    ("git remote set-head", &[]),
    // git refuses every other mode of `branch` beside `--list` (#4162).
    ("git branch --list", &[]),
    ("git config --get", &[]),
    ("git config --get-all", &[]),
    ("git config --get-regexp", &[]),
    ("git worktree list", &[]),
    ("git worktree add", &[]),
    ("xtask worktree add", &[]),
    ("xtask worktree whose", &[]),
    // `track work` runs sessions in the checkout it stands in.
    ("xtask track", &["work"]),
    ("xtask status", &[]),
    ("xtask ticket read", &[]),
];

/// `git symbolic-ref`'s options that leave the ref it names as it was.
const READS_REF: [&str; 5] = ["-q", "--quiet", "--short", "--recurse", "--no-recurse"];

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
    let placed = placed(Path::new(""), line, Places::Every).into_iter();
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

/// Whether `argv` can change the checkout it runs in (#3621, #3755): its
/// working tree, its index, its HEAD or its operation files. Every command can but a reader that
/// leaves the index alone ([`READERS`] less git's `add` and `commit`),
/// `sed` without `-i`, what [`LEAVES`] names, a `git symbolic-ref` that
/// reads ([`reads_ref`]) and an `xtask` that asks for its help; a
/// redirection the line makes is the line's, and read beside its commands.
pub(super) fn changes(argv: &Argv) -> bool {
    let index = argv.runs("git add") || argv.runs("git commit");
    let leaves = || {
        LEAVES
            .iter()
            .any(|(phrase, writing)| argv.runs(phrase) && !carries(argv, writing))
            || reads_ref(argv)
            || (argv.runs("xtask") && argv.asks_help())
    };
    index || !(operands_of(argv) == Operands::None || leaves())
}

/// Whether `argv` is a `git symbolic-ref` that reads the one ref it names
/// (#4066), its options all [`READS_REF`]: with a second operand it points
/// the first, `HEAD` among them, at it, and `-d` deletes the first.
fn reads_ref(argv: &Argv) -> bool {
    argv.after("git symbolic-ref").is_some_and(|own| {
        let (options, operands): (Vec<&String>, _) = own.iter().partition(|w| w.starts_with('-'));
        operands.len() == 1 && options.iter().all(|o| READS_REF.contains(&o.as_str()))
    })
}

/// Whether one of `argv`'s words is one of `options` or begins with one.
fn carries(argv: &Argv, options: &[&str]) -> bool {
    argv.words()
        .iter()
        .any(|w| options.iter().any(|option| w.starts_with(option)))
}

/// Commands beyond what [`changes`] reads as leaving a checkout alone that
/// leave its diff as it was (#3619): the builds and instruments whose
/// writes stay under `target/` and in the store; `cargo xtask` reads as
/// `xtask`.
const STILL: [&str; 8] = [
    "uniq",
    "git worktree",
    "cargo build",
    "cargo check",
    "cargo test",
    "cargo clippy",
    "cargo tree",
    "xtask",
];

/// The instruments and options of [`STILL`]'s that write tracked files.
const WRITING: [&str; 8] = [
    "regions", "steer", "generate", "new-ext", "new-sapi", "--accept", "--write", "--fix",
];

/// Whether `line` leaves every checkout's diff as it was, so the Stop does
/// not hold its session for it (#3618 ruling 1): it redirects only to a
/// device, a variable's directory or a path outside the directory the
/// checkouts of `root` stand in, and runs only what does not [`change`]
/// one, less a program text it interprets, and [`STILL`]. What it cannot
/// be read to leave alone it writes, the side that holds.
///
/// [`change`]: changes
pub(super) fn reads_only(line: &Line, root: &Path) -> bool {
    let beside = root.parent().unwrap_or(root);
    let elsewhere = |target: &String| {
        target.starts_with("/dev/")
            || target.starts_with('$')
            || (target.starts_with('/') && !Path::new(target).starts_with(beside))
    };
    line.written().iter().all(elsewhere)
        && line.commands().iter().all(|argv| {
            let words = argv.words();
            let still = STILL.iter().any(|phrase| argv.runs(phrase))
                && !words.iter().any(|w| WRITING.contains(&w.as_str()));
            words.is_empty() || still || !(changes(argv) || interprets(argv))
        })
}

/// Which of the command's words it can write.
fn operands_of(argv: &Argv) -> Operands {
    let words = argv.words();
    if READERS
        .iter()
        .any(|(phrase, writing)| argv.runs(phrase) && !carries(argv, writing))
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

    /// #3621: what a command can change in the checkout it runs in. git's
    /// staging and committing write the index and the history though they
    /// write no file; what reads, moves the shell or reaches the tracker
    /// changes nothing.
    #[test]
    fn a_command_changes_the_checkout_unless_it_reads_it() {
        let changes = |line: &str| Line::read(line).commands().iter().any(changes);
        for line in [
            "git add -- a",
            "git commit -F m -- a",
            "git merge --abort",
            "git -C wt checkout -- a",
            "git stash",
            "sed -i '' -e s/a/b/ a",
            "cargo xtask test --crate xtask",
            "python3 -c 'print(1)'",
            "touch a",
            "rm -f a",
            "gh pr checkout 1",
            "sort -o a b",
            "git checkout -b t1",
            "git config branch.t1.ticket 1",
            "git worktree remove wt",
            "cargo xtask track work --track x",
            "cargo xtask worktree own",
            // #4066: what can point the checkout's HEAD elsewhere or delete it.
            "git symbolic-ref HEAD refs/heads/x",
            "git symbolic-ref -m why HEAD refs/heads/x",
            "git symbolic-ref -d HEAD",
            "git symbolic-ref --delete HEAD",
            "git symbolic-ref -qd HEAD",
            // #4160, #4162.
            "cargo xtask ticket close 4160 --opens-nothing",
            "for n in a b; do touch $n; done",
            "find . -name '*.orig' -delete",
            "find . -exec rm {} +",
            "find . -execdir rm {} ;",
            "find . -okdir rm {} ;",
            "find . -fprint list",
            "git branch -D t1",
            "git -C wt branch -m t2",
            "git branch t3",
        ] {
            assert!(changes(line), "{line}");
        }
        for line in [
            "git status --short",
            "git log --oneline -3",
            "git -C wt diff --cached",
            "git rev-parse HEAD",
            "cat a | head -3",
            "grep -n x a",
            "sed -n 1,5p a",
            "cd /tmp",
            "gh issue view 3621",
            "echo x",
            // #3755: what writes only what checkouts share.
            "git worktree list --porcelain",
            "git -C wt worktree add -q -b t1 ../t1",
            "git config --get branch.t1.ticket",
            "git config --get-regexp '^branch'",
            "cargo xtask worktree add t1 --ticket 1",
            "cargo xtask worktree whose",
            "cargo xtask track",
            "cargo xtask status --section git",
            // #4066: what writes only `refs/remotes/<name>/HEAD`, or reads a ref.
            "git remote set-head origin main",
            "git remote set-head origin --auto",
            "git remote set-head origin -d",
            "git symbolic-ref refs/remotes/origin/HEAD",
            "git -C wt symbolic-ref -q --short HEAD",
            // #4160, #4162.
            "cargo xtask ticket read 4160",
            "for n in 4160 4162; do gh issue view $n; done",
            "find . -name '*.rs' -type f -newer a",
            "git branch --list",
            "git -C wt branch -a --list 't*'",
            "cargo xtask --help",
            "cargo xtask budget --help",
            "cargo xtask ticket close -h",
        ] {
            assert!(!changes(line), "{line}");
        }
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

    /// What the Stop holds a session for (#3619): a line that only asks,
    /// builds or measures leaves every diff as it was, and one that writes,
    /// commits, interprets or redirects into the checkouts' directory does
    /// not.
    #[test]
    fn a_line_that_asks_builds_or_measures_reads_only_and_one_that_writes_does_not() {
        let root = Path::new("/p/dax-php");
        let reads = |line: &str| reads_only(&Line::read(line), root);
        for line in [
            "git status --short | head -3",
            "cd /p/dax-php-worktrees/t1 && git log --oneline -3; cat seams.toml",
            "cargo xtask test --crate xtask > $S/out.txt 2>&1",
            "gh issue view 3619 > /tmp/issue.md",
            "sed -n 1,20p x.rs; grep -n fn y.rs 2>/dev/null",
        ] {
            assert!(reads(line), "{line}");
        }
        for line in [
            "sed -i '' s/a/b/ x.rs",
            "cargo fmt -p xtask",
            "git add -- x.rs && git commit -F m -- x.rs",
            "python3 - <<'EOF'\nopen('x','w')\nEOF",
            "echo x > /p/dax-php-worktrees/t1/x.rs",
            "cargo xtask witness --accept --crate x",
        ] {
            assert!(!reads(line), "{line}");
        }
    }
}
