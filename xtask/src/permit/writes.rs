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
//! ([`Walk::placed`]), it is that path ([`Place::is`]); a program names it
//! when the file's name stands in its text ([`names_file`]). What none of
//! that shows, a binary that holds the path where no word or file spells
//! it, the line cannot be read to write, and the sensor keeps the change.
//!
//! Whether a command can change the checkout it runs in at all is the
//! other question asked here ([`changes`], #3621), and where each write a
//! line makes lands is the third ([`of`], #4250): what a rule with
//! `writes = true` fires on and the owner lock judges, so that one
//! session's writes in a checkout another owns are refused and its reads,
//! and its writes elsewhere, are not.

use std::path::{Path, PathBuf};

use super::argv::{Argv, Line};
use super::shell::{Places, Via, Walk, at, moved};

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

/// Where each write of the line `walk` reads lands, read from where it
/// starts (#4250, Mitchell's
/// rulings of 2026-11-06 on c9.b and c9.c): the places a rule with `writes
/// = true` judges.
///
/// - A command that can change the checkout it runs in ([`changes`]) and
///   that [`LANDS`] lists lands on what it names, each word read where it
///   runs ([`moved`]), and on each place its own words reach besides
///   ([`Via::Reaches`]), and not where it runs: `mkdir -p /tmp/x` lands in
///   `/tmp/x` wherever the shell stands. A word the line does not resolve,
///   one led by a variable it never assigned, and a listed command that
///   names nothing, land where the command runs, as an unlisted one does.
/// - Every other such command lands where it runs and where its words
///   reach ([`Walk::placed`]), as the owner lock asked before.
/// - A redirection lands on its target, read where the shell of its own
///   command stands, or where the line stands for one on no command; a
///   target still led by a variable or a `~`, a descriptor, a device and
///   what a substitution prints are no place ([`Redirect`]).
///
/// [`Redirect`]: super::argv::Redirect
pub(super) fn of(walk: &Walk) -> Vec<PathBuf> {
    let Some(line) = walk.line() else {
        return Vec::new();
    };
    let mut places = Vec::new();
    for argv in line.commands().iter().filter(|argv| changes(argv)) {
        let lands = LANDS.iter().find(|row| row.lists(argv));
        let named = lands.and_then(|row| row.names(argv));
        let named = named.filter(|words| !words.is_empty() && words.iter().all(|w| resolved(w)));
        for shell in walk.stood(argv) {
            for (dir, via) in moved(&shell, argv) {
                match (&named, via) {
                    (Some(words), Via::Runs) => places.extend(words.iter().map(|w| at(&dir, w))),
                    _ => places.push(dir),
                }
            }
        }
    }
    for redirect in line.redirects() {
        if !resolved(&redirect.target) || is_no_file(&redirect.target) {
            continue;
        }
        let command = redirect.command.and_then(|n| line.commands().get(n));
        let shells = command.map_or_else(|| vec![walk.runs_in()], |c| walk.stood(c));
        places.extend(shells.iter().map(|shell| at(shell, &redirect.target)));
    }
    let mut once = Vec::new();
    for place in places {
        if !once.contains(&place) {
            once.push(place);
        }
    }
    once
}

/// Whether a word says a place: not empty, as what a substitution prints
/// reads, and led by neither a variable the line never assigned nor a
/// `~` it did not expand.
fn resolved(word: &str) -> bool {
    !word.is_empty() && !word.starts_with(['$', '~'])
}

/// Whether a redirection's target is no file of a checkout: a descriptor
/// (`>&2`, `>&-`) or a device.
fn is_no_file(target: &str) -> bool {
    target.starts_with(['&', '-'])
        || target.starts_with("/dev/")
        || target.chars().all(|c| c.is_ascii_digit())
}

/// One program the owner lock places by what it names (#4250): its phrase,
/// the words one of which it needs to write at all (`sed -i`, `find
/// -delete`), the words whose presence leave it unlisted, since what they
/// make it run its words do not name (`find -exec`), its options that take
/// the word after them as their value, and which of its operands it writes.
/// A row is added with a fixture each way under
/// `xtask/fixtures/rules/checkout.owner/` (Round 2, Q35.9).
struct Lands {
    phrase: &'static str,
    needs: &'static [&'static str],
    unless: &'static [&'static str],
    valued: &'static [&'static str],
    writes: Writes,
}

/// Which operands a listed program writes.
#[derive(Clone, Copy)]
enum Writes {
    /// Every one.
    All,
    /// The last, a copy's destination, or `-t`'s directory.
    Last,
    /// Every one past a program text, `sed`'s or `perl`'s, where no `-e`
    /// or `-f` gives one.
    PastScript,
    /// The start points before its expression, `find`'s, `.` for none.
    Roots,
    /// `of=`'s value, `dd`'s.
    Of,
    /// The directory a template names, `mktemp`'s, or `-p`'s; the
    /// temporary directory for none.
    Template,
    /// The directory, else the repository's name, `git clone`'s, and
    /// `--separate-git-dir`'s value.
    Clone,
}

/// The programs placed by what they name (Round 1, Q19 (a)): what writes
/// exactly the files and directories its words name.
const LANDS: [Lands; 16] = [
    Lands::row("mkdir", &["-m", "--mode"], Writes::All),
    Lands::row("rmdir", &[], Writes::All),
    Lands::row("rm", &[], Writes::All),
    Lands::row(
        "touch",
        &["-t", "-d", "-r", "-A", "--date", "--reference"],
        Writes::All,
    ),
    Lands::row("tee", &[], Writes::All),
    Lands::row(
        "truncate",
        &["-s", "-r", "--size", "--reference"],
        Writes::All,
    ),
    Lands::row("mv", &["-t", "--target-directory"], Writes::All),
    Lands::row("cp", &["-t", "--target-directory"], Writes::Last),
    Lands::row("ln", &["-t", "--target-directory"], Writes::Last),
    Lands::row(
        "install",
        &[
            "-t",
            "-m",
            "-o",
            "-g",
            "-f",
            "-B",
            "-S",
            "--target-directory",
            "--mode",
            "--owner",
            "--group",
        ],
        Writes::Last,
    ),
    Lands::row("dd", &[], Writes::Of),
    Lands::row(
        "mktemp",
        &["-p", "-t", "--tmpdir", "--suffix"],
        Writes::Template,
    ),
    Lands {
        needs: &["-i", "--in-place"],
        ..Lands::row(
            "sed",
            &["-e", "-f", "-l", "-i", "--expression", "--file"],
            Writes::PastScript,
        )
    },
    Lands {
        needs: &["-i"],
        ..Lands::row("perl", &[], Writes::PastScript)
    },
    Lands {
        needs: &["-delete"],
        unless: &["-exec", "-ok", "-fprint", "-fls"],
        ..Lands::row("find", &["-f"], Writes::Roots)
    },
    Lands::row(
        "git clone",
        &[
            "-b",
            "-o",
            "-u",
            "-c",
            "-j",
            "--branch",
            "--origin",
            "--upload-pack",
            "--reference",
            "--reference-if-able",
            "--depth",
            "--shallow-since",
            "--shallow-exclude",
            "--config",
            "--template",
            "--jobs",
            "--filter",
            "--server-option",
            "--separate-git-dir",
            "--bundle-uri",
            "--ref-format",
        ],
        Writes::Clone,
    ),
];

impl Lands {
    const fn row(phrase: &'static str, valued: &'static [&'static str], writes: Writes) -> Lands {
        Lands {
            phrase,
            needs: &[],
            unless: &[],
            valued,
            writes,
        }
    }

    /// Whether the row places `argv`: its phrase runs, with a word it needs
    /// (`perl`'s `-i` in a cluster of its switches) and none it is unless.
    fn lists(&self, argv: &Argv) -> bool {
        let needs = match self.writes {
            Writes::PastScript if self.phrase == "perl" => perl(argv).in_place,
            Writes::PastScript => argv.words().iter().skip(1).any(|w| in_place(w)),
            _ => self.needs.is_empty() || carries(argv, self.needs),
        };
        argv.runs(self.phrase) && needs && !carries(argv, self.unless)
    }

    /// The words `argv` writes, as the line expands them ([`Argv::named`]);
    /// `None` where its words do not say.
    fn names(&self, argv: &Argv) -> Option<Vec<String>> {
        let own = argv.words().len() - argv.after(self.phrase)?.len();
        let named = argv.named().get(own.checked_sub(1)?..)?;
        if self.phrase == "perl" {
            let perl = perl(argv);
            let files = named.get(perl.files..)?;
            return Some(files.to_vec());
        }
        let (operands, options) = read_options(named, self.valued);
        let option = |names: &[&str]| {
            options
                .iter()
                .filter(|(o, _)| names.contains(&o.as_str()))
                .map(|(_, v)| v.clone())
                .collect::<Vec<String>>()
        };
        let mut written = match self.writes {
            Writes::All => operands.clone(),
            Writes::Last if !option(&["-t", "--target-directory"]).is_empty() => {
                option(&["-t", "--target-directory"])
            }
            Writes::Last if self.phrase == "install" && argv.words().iter().any(|w| w == "-d") => {
                operands.clone()
            }
            Writes::Last => operands.last().cloned().into_iter().collect(),
            Writes::PastScript => {
                let scripted = !option(&["-e", "-f", "--expression", "--file"]).is_empty();
                operands.get(usize::from(!scripted)..)?.to_vec()
            }
            Writes::Roots => {
                let mut words = named.iter().peekable();
                let mut roots = Vec::new();
                while let Some(word) =
                    words.next_if(|w| FIND_LEADING.contains(&w.as_str()) || *w == "-f")
                {
                    if word == "-f" {
                        roots.extend(words.next().cloned());
                    }
                }
                roots.extend(
                    words
                        .take_while(|w| !w.starts_with(['-', '(', '!']))
                        .cloned(),
                );
                if roots.is_empty() {
                    roots.push(".".to_owned());
                }
                roots
            }
            Writes::Of => named
                .iter()
                .filter_map(|w| w.strip_prefix("of="))
                .map(str::to_owned)
                .collect(),
            Writes::Template => {
                let dir = option(&["-p", "--tmpdir"]);
                let temporary = || std::env::temp_dir().to_string_lossy().into_owned();
                match (dir.first(), operands.is_empty()) {
                    (Some(dir), _) => vec![dir.clone()],
                    (None, true) => vec![temporary()],
                    (None, false) if !option(&["-t"]).is_empty() => vec![temporary()],
                    (None, false) => operands.clone(),
                }
            }
            Writes::Clone => {
                let mut written = option(&["--separate-git-dir"]);
                match operands.as_slice() {
                    [_, dir, ..] => written.push(dir.clone()),
                    [repository] => written.extend(cloned_name(repository)),
                    [] => return None,
                }
                written
            }
        };
        if !matches!(self.writes, Writes::Clone | Writes::Template) {
            written.retain(|w| w != "-");
        }
        Some(written)
    }
}

/// `find`'s options before its start points, BSD's and GNU's; `-f` takes a
/// start point after it.
const FIND_LEADING: [&str; 8] = ["-H", "-L", "-P", "-E", "-X", "-d", "-s", "-x"];

/// The directory `git clone <repository>` makes where it runs: the
/// repository's last part less its `.git`.
fn cloned_name(repository: &str) -> Option<String> {
    let last = repository.trim_end_matches('/').rsplit(['/', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    (!name.is_empty()).then(|| name.to_owned())
}

/// A command's operands and its options with their values, read past its
/// program: an option in `valued` takes the rest of its cluster, its `=`
/// value or the word after it; `--` ends the options, and an option may
/// stand after an operand, as GNU's programs read it.
fn read_options(words: &[String], valued: &[&str]) -> (Vec<String>, Vec<(String, String)>) {
    let (mut operands, mut options) = (Vec::new(), Vec::new());
    let mut words = words.iter();
    while let Some(word) = words.next() {
        if word == "--" {
            operands.extend(words.by_ref().cloned());
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            let name = format!("--{name}");
            let value = match value {
                Some(value) => Some(value.to_owned()),
                None if valued.contains(&name.as_str()) => words.next().cloned(),
                None => None,
            };
            options.push((name, value.unwrap_or_default()));
            continue;
        }
        let Some(cluster) = word.strip_prefix('-').filter(|c| !c.is_empty()) else {
            operands.push(word.clone());
            continue;
        };
        for (n, c) in cluster.char_indices() {
            let option = format!("-{c}");
            if valued.contains(&option.as_str()) {
                let rest = &cluster[n + c.len_utf8()..];
                let value = if rest.is_empty() {
                    words.next().cloned().unwrap_or_default()
                } else {
                    rest.to_owned()
                };
                options.push((option, value));
                break;
            }
            options.push((option, String::new()));
        }
    }
    (operands, options)
}

/// What `perl`'s switches say of its words: whether `-i` edits in place,
/// and where the files it reads, and so writes, start among
/// [`Argv::named`] (#4250). A switch cluster ends at `-e` or `-E`, whose
/// program is the rest or the next word, and at a switch that takes the
/// rest as its value (`-i.bak`, `-Ilib`, `-Mstrict`); the first word past
/// the switches is the program's file where no `-e` gave one.
struct Perl {
    in_place: bool,
    files: usize,
}

fn perl(argv: &Argv) -> Perl {
    let named = argv.named();
    let (mut in_place, mut scripted, mut at) = (false, false, 0);
    while let Some(word) = named.get(at) {
        at += 1;
        if word == "--" {
            break;
        }
        let Some(cluster) = word
            .strip_prefix('-')
            .filter(|c| !c.is_empty() && !c.starts_with('-'))
        else {
            if word.starts_with("--") {
                continue;
            }
            at -= 1;
            break;
        };
        for (n, c) in cluster.char_indices() {
            match c {
                'e' | 'E' => {
                    scripted = true;
                    at += usize::from(cluster.len() == n + 1);
                    break;
                }
                'i' => {
                    in_place = true;
                    break;
                }
                'I' | 'M' | 'm' | 'x' | 'C' | 'l' | '0' | 'd' | 'D' => break,
                _ => {}
            }
        }
    }
    Perl {
        in_place,
        files: at + usize::from(!scripted),
    }
}

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
    // where the call stands, as [`Walk::placed`] reads it.
    let placed = Walk::of(Path::new(""), Some(line)).placed(Places::Every);
    let placed = placed.into_iter();
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
    line.written().any(|target| is(target))
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
    line.written().all(elsewhere)
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

    /// Where `line`'s writes land from `/usr`, a directory every machine
    /// has, as paths.
    fn lands(line: &str) -> Vec<String> {
        let line = Line::read(line);
        let places = of(&Walk::of(Path::new("/usr"), Some(&line)));
        places
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    /// #4250 (Round 1, Q19 (a)): a listed program lands on what it names,
    /// read where it runs, and not where it runs; a word the line does not
    /// resolve, and a listed program that names nothing, land where it
    /// runs, as every other program does.
    #[test]
    fn a_listed_program_lands_on_what_it_names_and_any_other_where_it_runs() {
        assert_eq!(lands("mkdir -p /tmp/x a"), ["/tmp/x", "/usr/a"]);
        assert_eq!(lands("cd /tmp && rm -rf /usr/x"), ["/usr/x"]);
        assert_eq!(lands("S=/tmp/s; mkdir -p \"$S/out\""), ["/tmp/s/out"]);
        assert_eq!(lands("mkdir -p \"$U/out\""), ["/usr"]);
        assert_eq!(lands("env -C /tmp mkdir d"), ["/tmp/d"]);
        assert_eq!(lands("cp -r /usr/a /tmp/b"), ["/tmp/b"]);
        assert_eq!(lands("cp -t /tmp a b"), ["/tmp"]);
        assert_eq!(lands("mv /usr/a /tmp/b"), ["/usr/a", "/tmp/b"]);
        assert_eq!(lands("touch -t 202611070000 /tmp/a"), ["/tmp/a"]);
        assert_eq!(lands("echo x | tee -a /tmp/a"), ["/tmp/a"]);
        assert_eq!(lands("echo x | tee"), ["/usr"]);
        assert_eq!(lands("dd if=/usr/a of=/tmp/b"), ["/tmp/b"]);
        assert_eq!(lands("install -m 644 a /tmp/b"), ["/tmp/b"]);
        assert_eq!(lands("mktemp -d /tmp/x.XXXX"), ["/tmp/x.XXXX"]);
        assert_eq!(lands("sed -i '' -e s/a/b/ /tmp/f"), ["/tmp/f"]);
        assert_eq!(
            lands("sed -i.bak s/a/b/ /tmp/f /tmp/g"),
            ["/tmp/f", "/tmp/g"]
        );
        assert_eq!(lands("sed -i s/a/b/"), ["/usr"]);
        assert_eq!(lands("perl -pi -e 's/a/b/' /tmp/f"), ["/tmp/f"]);
        assert_eq!(lands("perl -i.orig -pe 's/a/b/' -- /tmp/f"), ["/tmp/f"]);
        assert_eq!(lands("perl -i fix.pl /tmp/f"), ["/tmp/f"]);
        assert_eq!(lands("find /tmp/d -name '*.o' -delete"), ["/tmp/d"]);
        assert_eq!(lands("find -L /tmp/d -delete"), ["/tmp/d"]);
        assert_eq!(lands("find . -delete"), ["/usr/."]);
        assert_eq!(lands("find /tmp/d -exec rm {} +"), ["/usr"]);
        assert_eq!(lands("git -C /tmp clone -q --depth 1 u.git d"), ["/tmp/d"]);
        assert_eq!(lands("git clone https://h/o/r.git"), ["/usr/r"]);
        assert_eq!(lands("touch /tmp/a && cargo fmt"), ["/tmp/a", "/usr"]);
        assert!(lands("cat /usr/a; git status; sed -n 1p /usr/a").is_empty());
        // Q35.6: a directory the line makes and then enters is not one it
        // can enter as it is judged, so what follows the change's `;` may
        // run where the line started, and lands there too.
        assert_eq!(
            lands("mkdir -p /tmp/t4250-never && cd /tmp/t4250-never; touch a"),
            ["/tmp/t4250-never", "/tmp/t4250-never/a", "/usr/a"]
        );
    }

    /// #4250 (Round 1, Q20 (a)): a redirection lands on its target read
    /// where its own command's shell stands, a variable the line assigned
    /// and `~/` expanded; one on no command where the line stands; and a
    /// target the line still does not resolve, a descriptor and a device,
    /// nowhere.
    #[test]
    fn a_redirection_lands_where_its_own_command_stands() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(lands("ls; cd /tmp; echo x > y"), ["/tmp/y"]);
        assert_eq!(lands("cd /tmp; ls; cd /usr; echo x > y"), ["/usr/y"]);
        assert_eq!(lands("R=/usr; cd /tmp && echo x >> $R/a"), ["/usr/a"]);
        assert_eq!(lands("cd /tmp && echo x > ~/y"), [format!("{home}/y")]);
        assert_eq!(lands("(cd /tmp && ls) > out"), ["/usr/out"]);
        assert_eq!(lands("git -C /tmp log > out"), ["/usr/out"]);
        // An interpreter stays where it runs; what its program redirects
        // lands where that program's command stands.
        assert_eq!(lands("bash -c 'cd /tmp && echo x > y'"), ["/usr", "/tmp/y"]);
        assert!(lands("echo x > \"$UNSET/a\"; echo y >&2; echo z > /dev/null 2>&1").is_empty());
        assert!(lands("echo x > $(cat /usr/f)").is_empty());
    }

    /// Round 2, Q35.9: each row of [`LANDS`] carries a fixture of the owner
    /// lock each way, `allow-lands-<row>.json` where what it names stands
    /// outside the owned checkout and `deny-lands-<row>.json` where it
    /// stands inside.
    #[test]
    fn each_listed_program_has_a_fixture_each_way() {
        let dir = crate::package_dir().join("fixtures/rules/checkout.owner");
        for row in &LANDS {
            let name = row.phrase.replace(' ', "-");
            for way in ["allow", "deny"] {
                let fixture = dir.join(format!("{way}-lands-{name}.json"));
                assert!(fixture.is_file(), "{}", fixture.display());
            }
        }
    }
}
