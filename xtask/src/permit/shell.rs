//! Where a shell line's commands run: the one walk over its directory
//! changes, its subshells and the moves its programs' own words make
//! ([`Walk`]), built once for every rule that judges a call (Round 3,
//! Q28 (e)), and the readings taken from it, which the rules scoped by
//! `where`, the owner lock, the write sensor and `permit --after` all
//! read, so they move together: where the line stands, where each
//! command runs and reaches, where the call acts and the checkout it runs
//! in, and the command it cannot place.

use std::fs;
use std::path::{Path, PathBuf};

use super::argv::{Argv, Joint, Line, Move};
use super::rules::regex;
use crate::git;

/// Which of a command's places a reader asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Places {
    /// Every place it runs or reaches.
    Every,
    /// The places git reads a repository in: where it runs, unless its
    /// words name a git dir ([`Move::GitDir`]), which git reads in its
    /// place, and every place it reaches besides. Under `GIT_DIR` the
    /// index and an operation's marker are the git dir's, and the
    /// repository where the command runs is not read at all (#3271).
    Repositories,
}

impl Places {
    /// Whether these places take one `argv` stands at `via`.
    fn keeps(self, argv: &Argv, via: Via) -> bool {
        let named = || argv.moves().iter().any(|m| matches!(m, Move::GitDir(_)));
        self == Places::Every || via == Via::Reaches || !named()
    }
}

/// How a command stands at a place the walk gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Via {
    /// It runs there, and reaches it by no word of its own.
    Runs,
    /// A word of its own reaches it ([`Move::Reaches`], [`Move::GitDir`]).
    Reaches,
}

/// A command a rule fires on that the walk places nowhere, and the change
/// or the program past which it lost the line ([`Walk::unplaced`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unplaced {
    pub(super) command: String,
    pub(super) past: String,
}

/// The one reading of where a shell line's commands run, from which
/// every method here is read, so
/// `where = "ticketed"`, `where = "main"`, `where = "staging"`,
/// `where = "operating"`, `where = "checkout"`, the checkout `permit --after` holds and the
/// approvals it reads move together (#2597, #2607, #2995).
///
/// Each `cd`, `pushd` and `popd` the line says the target of moves the
/// shell it runs in, from where the call starts, `~/` being the home; a `cd`
/// or `pushd` to a variable the line assigned before it, `W=/wt && cd
/// "$W"`, goes where the value names ([`Argv::directory`]). A change may
/// not run, past `||` or past a command of its `&&` chain that may fail,
/// and may fail, into a place the shell cannot enter as the line is
/// judged ([`enters`]): a file, a directory without search permission or
/// none. The shell then stays where it stood: each shell keeps the places
/// it may stand where every command since the last separator other than
/// `&&` succeeded apart from those where one of them failed, and a command
/// after any other separator ([`Joint`]) runs at both, so where `/wt` is
/// no directory, `cd /wt && x; y` runs `x` in `/wt` and `y` in `/wt` and
/// where the line started, as `x || cd /wt; y` runs `y` wherever `/wt`
/// is. A change sure to run into a directory it can enter is sure to move
/// the shell, so `cd <another repository> && S=x; git commit` commits
/// there alone (#3257). What that leaves open, stated: a directory the
/// line itself makes, removes or makes unsearchable before its change, or
/// one the shell's sandbox refuses where permit's process may enter it,
/// is read as it stands when the line is read, before the call and again
/// at `permit --after`, so the two may place it apart. A change piped or
/// sent to the background runs in a shell of its own and moves nothing. A
/// change inside a subshell, a substitution or a child shell moves the
/// commands in it alone ([`Line::shells`], #2764), and never the line:
/// `(cd /other && git status); git add x` stands where it started, and
/// its `git add` runs there (#2787).
///
/// A change the line does not say where it goes is not followed, and the
/// line is lost there: a variable the line did not assign, `cd` with no
/// operand or `-`, `pushd` with an option, a stack place (`+1`) or none,
/// and `popd` with an option or no `pushd` before it; nothing past it in
/// that shell is placed. A program moved by its own words runs where they
/// say ([`Move`]), and one moved where they do not say loses the line.
#[derive(Default)]
pub(super) struct Walk<'a> {
    /// Where the line's shell starts.
    start: PathBuf,
    /// The line walked; `None` for a call that runs none.
    line: Option<&'a Line>,
    /// Each command with each place it may run or reach, one per place;
    /// none for a change, nor for a command past a change its shell lost.
    placed: Vec<Placed<'a>>,
    /// Where the line's own shell may stand at its first command past
    /// its leading changes and its `export`s of assignments alone, which
    /// move nothing, a subshell's changes among them.
    stands: Vec<PathBuf>,
    /// The commands from that one on.
    rest: &'a [Argv],
    /// Whether the line makes a change, or moves a program, where it does
    /// not say.
    lost: bool,
    /// The first such change or program, as the line spells it: what a
    /// denial names when it cannot place a command past it.
    lost_by: Option<String>,
    /// Each command its shell placed, with each place the shell may stand
    /// as it runs, before the command's own words move it: where its
    /// redirections land and its words are read (#4250).
    stood: Vec<(&'a Argv, Vec<PathBuf>)>,
}

/// One place the walk gives a command, and how it stands there, which
/// [`Places`] tells apart.
struct Placed<'a> {
    argv: &'a Argv,
    dir: PathBuf,
    via: Via,
}

/// One shell the walk is in: its id, the places it may stand where every
/// command of its `&&` chain succeeded, those where one of them failed,
/// its `pushd` stack, each entry the places it pushed, whether it lost,
/// whether every command of its chain so far is sure to have run and
/// succeeded, and the places, stack and loss it began its chain with,
/// which a chain sent to the background leaves it.
#[derive(Default)]
struct Shell {
    id: u32,
    sure: Vec<PathBuf>,
    failed: Vec<PathBuf>,
    pushed: Vec<Vec<PathBuf>>,
    lost: bool,
    certain: bool,
    chain: (Vec<PathBuf>, Vec<Vec<PathBuf>>, bool),
}

impl Shell {
    fn new(id: u32, sure: Vec<PathBuf>, lost: bool, certain: bool) -> Shell {
        let chain = (sure.clone(), Vec::new(), lost);
        Shell {
            id,
            sure,
            lost,
            certain,
            chain,
            ..Shell::default()
        }
    }

    /// Where a command joined by `before` runs: after `&&` or a pipe where
    /// the chain succeeded, and after any other separator wherever the
    /// shell may stand, which begins a chain, sure to run but past `||`.
    fn runs_at(&mut self, before: Joint) -> Vec<PathBuf> {
        if !matches!(before, Joint::And | Joint::Pipe) {
            for dir in std::mem::take(&mut self.failed) {
                add(&mut self.sure, dir);
            }
            self.chain = (self.sure.clone(), self.pushed.clone(), self.lost);
            self.certain = before != Joint::Or;
        }
        self.sure.clone()
    }

    /// The shell once `argv`, a `cd`, `pushd` or `popd`, ran at `from`:
    /// moved where it succeeded and standing where it failed, or lost
    /// where the line does not say where it goes.
    fn change(&mut self, from: Vec<PathBuf>, argv: &Argv) {
        let words = argv.words();
        let to = argv
            .directory()
            .filter(|to| !to.starts_with(['-', '+', '$']));
        let moved = |to: &str| Some(from.iter().map(|dir| at(dir, to)).collect());
        let moved = match (words.first().map(String::as_str), to) {
            (Some("cd"), Some(to)) => moved(to),
            (Some("pushd"), Some(to)) if words.len() == 2 => {
                self.pushed.push(from.clone());
                moved(to)
            }
            (Some("popd"), _) if words.len() == 1 => self.pushed.pop(),
            _ => None,
        };
        let Some(moved) = moved else {
            self.lost = true;
            return;
        };
        self.certain &= moved.iter().all(|dir| enters(dir));
        self.sure.clear();
        for dir in moved {
            add(&mut self.sure, dir);
        }
        if !self.certain {
            for dir in from {
                add(&mut self.failed, dir);
            }
        }
    }

    /// The shell as its chain began, the chain having run in a shell of
    /// its own in the background.
    fn background(&mut self) {
        (self.sure, self.pushed, self.lost) = self.chain.clone();
        self.failed.clear();
    }
}

impl<'a> Walk<'a> {
    /// The walk of `line` from `start`, where its shell starts.
    pub(super) fn of(start: &Path, line: Option<&'a Line>) -> Walk<'a> {
        let mut walk = Walk {
            start: start.to_path_buf(),
            line,
            ..Walk::default()
        };
        walk.stands.push(start.to_path_buf());
        let Some(line) = line else {
            return walk;
        };
        let mut top = Shell::new(0, walk.stands.clone(), false, true);
        let (mut nested, mut leading): (Vec<Shell>, _) = (Vec::new(), true);
        let commands = line.commands();
        let each = commands.iter().zip(line.shells()).zip(line.joints());
        for (n, ((argv, ids), joints)) in each.enumerate() {
            let kept = nested.iter().zip(ids).take_while(|(s, id)| s.id == **id);
            let kept = kept.count();
            if kept < nested.len() {
                nested.truncate(kept);
                // A shell that closed may have failed, and a change past it
                // in its chain not run.
                nested.last_mut().unwrap_or(&mut top).certain = false;
            }
            let entering = ids.len() > nested.len();
            for id in ids.get(nested.len()..).unwrap_or_default() {
                let parent = nested.last_mut().unwrap_or(&mut top);
                let sure = parent.runs_at(joints.before);
                let (lost, certain) = (parent.lost, parent.certain);
                if leading && nested.is_empty() {
                    walk.stands.clone_from(&sure);
                }
                nested.push(Shell::new(*id, sure, lost, certain));
            }
            let shell = nested.last_mut().unwrap_or(&mut top);
            let at = shell.runs_at(joints.before);
            if !shell.lost {
                walk.stood.push((argv, at.clone()));
            }
            let changes = changes_directory(argv);
            // A pipe's side runs in a shell of its own, unless the command
            // opens one, a subshell or a substitution, which its own
            // changes move.
            let apart = (joints.before == Joint::Pipe && !entering) || joints.after == Joint::Pipe;
            let words = argv.words();
            let exports = words.first().is_some_and(|w| w == "export")
                && words.iter().skip(1).all(|w| w.contains('='));
            if changes && !apart && !shell.lost {
                shell.change(at.clone(), argv);
                walk.lose(shell.lost, argv);
            } else if !exports {
                // It may fail, and a change past it in its chain not run.
                shell.certain = false;
            }
            walk.lose(argv.moves().contains(&Move::Unsaid), argv);
            for (dir, via) in at.iter().flat_map(|dir| moved(dir, argv)) {
                if changes || shell.lost {
                    continue;
                }
                let same = |p: &&mut Placed| std::ptr::eq(p.argv, argv) && p.dir == dir;
                match walk.placed.iter_mut().find(same) {
                    // A place it runs at and reaches is one it reaches.
                    Some(placed) if via == Via::Reaches => placed.via = via,
                    Some(_) => {}
                    None => walk.placed.push(Placed { argv, dir, via }),
                }
            }
            let read_past = exports || (changes && (apart || !shell.lost));
            let background = joints.after == Joint::Background;
            if background {
                shell.background();
            }
            if leading && ids.is_empty() {
                walk.stands = if background { shell.sure.clone() } else { at };
            }
            if leading && !read_past {
                leading = false;
                walk.rest = commands.get(n..).unwrap_or_default();
            }
        }
        if leading {
            walk.stands = top.sure;
        }
        walk
    }

    /// The walk lost where `argv` goes, when `lost` says it did.
    fn lose(&mut self, lost: bool, argv: &Argv) {
        if lost && !self.lost {
            self.lost_by = Some(argv.words().join(" "));
        }
        self.lost |= lost;
    }

    /// The line walked, `None` for a call that runs none.
    pub(super) fn line(&self) -> Option<&'a Line> {
        self.line
    }

    /// Whether the line makes a change, or moves a program, where it does
    /// not say.
    pub(super) fn lost(&self) -> bool {
        self.lost
    }

    /// Where the line stands: the one place its shell may stand at its
    /// first command past its leading changes ([`Walk::stands`]), or where
    /// it started where it may stand in more than one; which is how a
    /// session whose shell stands in one checkout runs a command in
    /// another.
    pub(super) fn runs_in(&self) -> PathBuf {
        match self.stands.as_slice() {
            [one] => one.clone(),
            _ => self.start.clone(),
        }
    }

    /// The commands of the line whose place is read, each with every place
    /// of it `asked` names, one pair per place ([`Walk::placed`]), a
    /// command past a later change read where that change took the shell,
    /// never where the shell started (#2720), as an orchestrator's `cat
    /// $S/owner; cd <main>; git merge --no-commit t1955` once was not
    /// (#2723).
    pub(super) fn placed(&self, asked: Places) -> Vec<(&'a Argv, PathBuf)> {
        let kept = self.placed.iter().filter(|p| asked.keeps(p.argv, p.via));
        kept.map(|p| (p.argv, p.dir.clone())).collect()
    }

    /// Each place the shell of `argv`, one of the line's commands, may
    /// stand as it runs ([`Walk::stood`]), before the command's own words
    /// move it, and where the line stands ([`Walk::runs_in`]) for one its
    /// shell placed nowhere, past a change the line does not say where it
    /// goes: the places [`super::writes::of`] reads a command's words and
    /// its redirections from (#4250).
    pub(super) fn stood(&self, argv: &Argv) -> Vec<PathBuf> {
        let found = self.stood.iter().find(|(a, _)| std::ptr::eq(*a, argv));
        found.map_or_else(|| vec![self.runs_in()], |(_, at)| at.clone())
    }

    /// Where each command of the line that `fires` may run or reach, the
    /// places `asked` names ([`Walk::placed`]), or, for one placed
    /// nowhere, past a change the line does not say where it goes, where
    /// the line stands moved by its own words ([`moved`]): a rule scoped
    /// by `where` judges the commands it fires on there, so `git -C /wt
    /// commit` is judged in `/wt`, `(cd /wt && ls); git add x` where the
    /// shell stands (#2764), and `cd /wt; git add x` both in `/wt` and
    /// where it started where `/wt` is no directory it can enter, the `cd`
    /// being one that may fail (#2995, #3257). None for a call with no
    /// line.
    pub(super) fn fired_in(&self, asked: Places, fires: impl Fn(&Argv) -> bool) -> Vec<PathBuf> {
        let mut checkouts = Vec::new();
        let commands = self.line.map(Line::commands).unwrap_or_default();
        for argv in commands.iter().filter(|argv| fires(argv)) {
            let placed = self.places_of(argv).map(|(dir, via)| (dir.clone(), via));
            let mut at: Vec<(PathBuf, Via)> = placed.collect();
            if at.is_empty() {
                at = moved(&self.runs_in(), argv);
            }
            let kept = at.into_iter().filter(|(_, via)| asked.keeps(argv, *via));
            for (dir, _) in kept {
                add(&mut checkouts, dir);
            }
        }
        checkouts
    }

    /// Where the call acts: where the line stands ([`Walk::runs_in`]), or,
    /// where every command past its leading directory changes is git and
    /// each runs in one and the same other place, by its `-C` or a
    /// subshell's change, that place, so `git -C /wt checkout --
    /// seams.toml` and `git -C /wt add x && git -C /wt commit` act in
    /// `/wt`. git's `-C` moves one command and not the shell, so a line any
    /// of whose other commands runs where the shell stands (`git -C /wt
    /// status && git checkout -- seams.toml`, `touch x && git -C /wt
    /// status`), or may run in more than one place, acts where the shell
    /// stands, the call being held in one checkout.
    pub(super) fn acts_in(&self) -> PathBuf {
        let dir = self.runs_in();
        let mut named = self.rest.iter().map(|argv| {
            let mut at = self.places_of(argv).map(|(dir, _)| dir);
            let one = at.next().filter(|_| at.next().is_none());
            one.filter(|_| argv.program() == "git")
        });
        match named.next() {
            Some(Some(first)) if *first != dir && named.all(|other| other == Some(first)) => {
                first.clone()
            }
            _ => dir,
        }
    }

    /// The checkout the call runs in: where it acts ([`Walk::acts_in`]),
    /// at the top of its worktree, a linked one's own; `fallback`, which
    /// the caller names, where that is in no checkout, as it is for a `cd`
    /// to a variable the line did not assign. What `permit --after` holds,
    /// and the one checkout it puts a file back in (#2597, #2607). A line
    /// whose commands run in several checkouts is held where its shell
    /// stands, since a `-C` moves one command and the call is held in one.
    pub(super) fn ran_in(&self, fallback: &Path) -> PathBuf {
        let top = git::rev_parse(&self.acts_in(), &["--show-toplevel"]);
        let top = top.map_or_else(|_| fallback.to_path_buf(), PathBuf::from);
        fs::canonicalize(&top).unwrap_or(top)
    }

    /// The first command of the line that `fires` and that the walk places
    /// in no checkout: one past a change the line does not say where it
    /// goes, or one moved by its own words where they do not say
    /// ([`Walk::lost`]). Where no command fires on its words alone, the
    /// line fired whole, as a pattern over a redirection does, and the
    /// first command placed nowhere that changes no directory is the one
    /// named, so the line is never judged where the shell started (#3625).
    pub(super) fn unplaced(&self, fires: impl Fn(&Argv) -> bool) -> Option<Unplaced> {
        let (line, past) = (self.line?, self.lost_by.clone()?);
        let lost = |argv: &&Argv| {
            self.places_of(argv).next().is_none() || argv.moves().contains(&Move::Unsaid)
        };
        let whole = !line.commands().iter().any(&fires);
        let command = line
            .commands()
            .iter()
            .filter(|argv| {
                if whole {
                    !changes_directory(argv)
                } else {
                    fires(argv)
                }
            })
            .find(lost)?;
        Some(Unplaced {
            command: command.words().join(" "),
            past,
        })
    }

    /// Whether the line spells a directory change it did not read as a
    /// command, one a program it does not peel runs (`xargs sh -c 'cd
    /// …'`): the one reading of a line's text where its commands cannot
    /// say.
    pub(super) fn spells_unread_changes(&self) -> bool {
        let Some(line) = self.line else {
            return false;
        };
        let read = line
            .commands()
            .iter()
            .filter(|argv| changes_directory(argv));
        let spelled = regex(r"(?:^|[^\w./-])(?:cd|pushd|popd)\b");
        spelled.find_iter(line.text()).count() > read.count()
    }

    /// Each place `argv`, one of the line's commands, may run or reach,
    /// and how it stands there.
    fn places_of(&self, argv: &'a Argv) -> impl Iterator<Item = (&PathBuf, Via)> {
        let placed = self
            .placed
            .iter()
            .filter(move |p| std::ptr::eq(p.argv, argv));
        placed.map(|p| (&p.dir, p.via))
    }
}

/// Whether `argv` changes the directory of the shell it runs in.
fn changes_directory(argv: &Argv) -> bool {
    matches!(argv.program(), "cd" | "pushd" | "popd")
}

/// Whether a change into `dir` succeeds as the line is judged: `dir/.`
/// resolves to a directory only where `dir` is one the shell may search,
/// as `cd` needs it to be (#3257).
fn enters(dir: &Path) -> bool {
    dir.join(".").is_dir()
}

/// `one` added to `set` where it is not there already.
fn add<T: PartialEq>(set: &mut Vec<T>, one: T) {
    if !set.contains(&one) {
        set.push(one);
    }
}

/// Where a command its shell runs in `dir` runs, moved by each
/// [`Move::Into`] of its words, then each place it reaches besides
/// ([`Move::Reaches`], [`Move::GitDir`]), read from where the moves into
/// left the program, as git reads `GIT_DIR` once its `-C` has moved it;
/// each with how it stands there.
pub(super) fn moved(dir: &Path, argv: &Argv) -> Vec<(PathBuf, Via)> {
    let mut runs = dir.to_path_buf();
    for one in argv.moves() {
        if let Move::Into(to) = one {
            runs = at(&runs, to);
        }
    }
    let reached = argv.moves().iter().filter_map(|one| match one {
        Move::Reaches(to) | Move::GitDir(to) => Some((at(&runs, to), Via::Reaches)),
        Move::Into(_) | Move::Unsaid => None,
    });
    let reached: Vec<(PathBuf, Via)> = reached.collect();
    std::iter::once((runs, Via::Runs)).chain(reached).collect()
}

/// `to` read as a path from `dir`, `~/` as the home.
pub(super) fn at(dir: &Path, to: &str) -> PathBuf {
    match (to.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => dir.join(to),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::scratch::Tree;

    /// Where the line stands, walked from `start`.
    fn runs_in(start: &Path, line: Option<&Line>) -> PathBuf {
        Walk::of(start, line).runs_in()
    }

    /// Where the call acts, walked from `start`.
    fn acts_in(start: &Path, line: Option<&Line>) -> PathBuf {
        Walk::of(start, line).acts_in()
    }

    /// Where each command of `line` that `fires` runs or reaches, walked
    /// from `start`.
    fn fired_in(
        start: &Path,
        line: &Line,
        asked: Places,
        fires: impl Fn(&Argv) -> bool,
    ) -> Vec<PathBuf> {
        Walk::of(start, Some(line)).fired_in(asked, fires)
    }

    /// #2787: a leading `cd` inside a subshell moves that subshell alone,
    /// so the line stands where it started, its `git add` runs and acts
    /// there and is judged there, and its `git status` is judged where the
    /// subshell's change took it alone; a line whose every git command
    /// runs in the subshell's checkout acts in it, and a change outside a
    /// subshell still moves the line.
    #[test]
    fn a_leading_change_inside_a_subshell_moves_that_subshell_alone() {
        let (root, other) = (Path::new("/shell"), Path::new("/other"));
        let line = Line::read("(cd /other && git status); git add x");
        assert_eq!(runs_in(root, Some(&line)), root);
        assert_eq!(acts_in(root, Some(&line)), root);
        let adds = |argv: &Argv| argv.words().get(1).is_some_and(|w| w == "add");
        assert_eq!(fired_in(root, &line, Places::Every, adds), [root]);
        let statuses = |argv: &Argv| argv.words().get(1).is_some_and(|w| w == "status");
        assert_eq!(fired_in(root, &line, Places::Every, statuses), [other]);
        let inside = Line::read("(cd /other && git add x && git commit -m y)");
        assert_eq!(runs_in(root, Some(&inside)), root);
        assert_eq!(acts_in(root, Some(&inside)), other);
        let outside = Line::read("cd /wt && (cd sub && ls); git add x");
        assert_eq!(runs_in(root, Some(&outside)), Path::new("/wt"));
        assert_eq!(acts_in(root, Some(&outside)), Path::new("/wt"));
    }

    /// #3271: git reads the repository its `GIT_DIR` or `--git-dir` names
    /// in place of the one where it runs, and every other reader still
    /// takes both; `GIT_WORK_TREE` and `--work-tree` change no index and
    /// stay places it reaches besides where it runs.
    #[test]
    fn git_reads_the_repository_its_git_dir_names_in_place_of_where_it_runs() {
        let (root, wt, git_dir) = (Path::new("/shell"), Path::new("/wt"), Path::new("/o/.git"));
        let read = |text: &str, asked: Places| {
            fired_in(root, &Line::read(text), asked, |argv| {
                argv.program() == "git"
            })
        };
        let repositories = |text: &str| read(text, Places::Repositories);
        assert_eq!(
            repositories("GIT_DIR=/o/.git git commit -F m -- x"),
            [git_dir]
        );
        assert_eq!(repositories("git --git-dir=/o/.git commit"), [git_dir]);
        assert_eq!(repositories("git --git-dir /o/.git commit"), [git_dir]);
        assert_eq!(
            repositories("export GIT_DIR=/o/.git; git commit"),
            [git_dir]
        );
        assert_eq!(repositories("GIT_DIR=.git git -C /o commit"), [git_dir]);
        assert_eq!(
            repositories("GIT_DIR=/o/.git GIT_WORK_TREE=/wt git add x"),
            [git_dir, wt]
        );
        assert_eq!(repositories("GIT_WORK_TREE=/wt git add x"), [root, wt]);
        assert_eq!(repositories("git --work-tree=/wt add x"), [root, wt]);
        assert_eq!(repositories("cd /a && GIT_DIR=/o/.git git x"), [git_dir]);
        // Where the git dir is where it runs, it is read there.
        assert_eq!(repositories("GIT_DIR=. git -C /o/.git log"), [git_dir]);
        // A variable the line did not assign says nowhere: it runs where
        // it runs, and the line is lost.
        assert_eq!(repositories("GIT_DIR=$X git commit"), [root]);
        let places = read("GIT_DIR=/o/.git git commit", Places::Every);
        assert_eq!(places, [root, git_dir]);
    }

    /// #2995: a change is followed only as far as the `&&` chain it
    /// stands in, since it may fail; one piped or sent to the background
    /// moves nothing; a program's own words move it.
    #[test]
    fn a_change_is_followed_as_far_as_its_and_chain_and_a_program_where_its_words_say() {
        let (root, wt) = (Path::new("/shell"), Path::new("/wt"));
        let fired = |text: &str, program: &str| {
            fired_in(root, &Line::read(text), Places::Every, |argv| {
                argv.program() == program
            })
        };
        assert_eq!(fired("cd /wt && x; y", "x"), [wt]);
        assert_eq!(fired("cd /wt && x; y", "y"), [wt, root]);
        assert_eq!(fired("cd /wt &&\n  y", "y"), [wt]);
        assert_eq!(fired("cd /wt && X=1; y", "y"), [wt, root]);
        assert_eq!(fired("cd /wt || true && y", "y"), [wt, root]);
        assert_eq!(fired("cd /wt | true; y", "y"), [root]);
        assert_eq!(fired("x | cd /wt && y", "y"), [root]);
        assert_eq!(fired("cd /wt & y", "y"), [root]);
        assert_eq!(fired("cd /wt; cd /a && y", "y"), [Path::new("/a")]);
        assert_eq!(fired("env -C /wt y", "y"), [wt]);
        // A lone `&` sends the whole `&&` list to the background; a pipe's
        // subshell is moved by its own change; `|&` is a pipe.
        assert_eq!(fired("cd /wt && x & y", "y"), [root]);
        assert_eq!(fired("x | (cd /wt; y)", "y"), [wt, root]);
        assert_eq!(fired("x |& cd /wt && y", "y"), [root]);
        assert_eq!(fired("env -iC /wt y; env -C/wt y", "y"), [wt]);
        let reached = [wt.to_path_buf(), wt.join(".git")];
        assert_eq!(fired("GIT_DIR=.git git -C /wt status", "git"), reached);
        assert_eq!(
            fired("cd /a && GIT_DIR=/wt git x", "git"),
            [Path::new("/a"), wt]
        );
        // #3257: a change sure to run into a directory it can enter is sure
        // to move the shell; one past `||` or past a command of its chain
        // that may fail may not run.
        let tmp = std::env::temp_dir();
        let t = tmp.display();
        assert_eq!(fired(&format!("cd / && cd {t}; y"), "y"), [tmp.as_path()]);
        assert_eq!(
            fired(&format!("x || cd {t}; y"), "y"),
            [tmp.as_path(), root]
        );
        assert_eq!(
            fired(&format!("x && cd {t}; y"), "y"),
            [tmp.as_path(), root]
        );
        assert_eq!(fired(&format!("x; (cd {t}; y)"), "y"), [tmp.as_path()]);
        assert_eq!(
            fired(&format!("(x) && cd {t}; y"), "y"),
            [tmp.as_path(), root]
        );
        let line = |text: &str| Line::read(text);
        assert_eq!(runs_in(root, Some(&line("cd /wt; git add x"))), root);
        assert_eq!(runs_in(root, Some(&line("cd /wt && x & git add y"))), root);
        assert_eq!(runs_in(root, Some(&line("cd /wt && x; git add x"))), wt);
        assert_eq!(acts_in(root, Some(&line("cd /a; git -C sub add x"))), root);
        assert_eq!(acts_in(root, Some(&line("env -C /wt git add x"))), wt);
        assert_eq!(acts_in(root, Some(&line("git -C /wt add x"))), wt);
        let walk = |text: &str| Walk::of(root, Some(&line(text))).lost();
        assert!(walk("cd $X; y") && walk("find . -execdir y {} +"));
        assert!(!walk("cd $X | true; y") && !walk("X=/wt && cd $X"));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(crate::scratch::UNHOOKED)
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {}", args.join(" "));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A repository of one commit on `main`, with the merge marker git
    /// writes when a merge stops before its commit.
    fn repository(name: &str) -> Tree {
        let dir = Tree::new(name).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
        dir
    }

    /// A linked worktree of `dir` at `linked`, on a branch of its own.
    fn worktree(dir: &Path, linked: &Path) {
        git(
            dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "t1",
                linked.to_str().unwrap(),
            ],
        );
    }

    /// The checkout a call ran in is its workspace moved by a leading
    /// `cd`, past the assignments before it, at the top of the worktree
    /// the `cd` names; a `cd` into no checkout leaves the workspace.
    #[test]
    fn a_call_ran_in_the_top_of_the_worktree_its_leading_cd_names() {
        let dir = repository("operation-ran-in");
        let base = Tree::new("operation-ran-in-linked").unwrap();
        let linked = base.join("t1");
        worktree(&dir, &linked);
        std::fs::create_dir(linked.join("sub")).unwrap();
        let top = linked.canonicalize().unwrap();
        let ran_in = |line: &str| Walk::of(&dir, Some(&Line::read(line))).ran_in(&dir);
        let at = linked.display();
        assert_eq!(ran_in(&format!("cd {at}/sub && touch x")), top);
        assert_eq!(ran_in(&format!("S=/tmp/s && cd {at} && touch x")), top);
        let exported = format!("export A=1 B=2 && cd {at} && touch x");
        assert_eq!(ran_in(&exported), top);
        let workspace = dir.canonicalize().unwrap();
        assert_eq!(ran_in("touch x"), workspace);
        assert_eq!(ran_in("cd /var/empty/none && touch x"), workspace);
    }

    /// #2607: a `cd` to a variable the line assigned before it, plainly or
    /// by `export`, and a `pushd` move the call to the worktree they name,
    /// and a `popd` returns to where the `pushd` left; git's `-C` does
    /// where every command past them is git and names one directory. A
    /// change the line cannot read leaves it where it stood, the
    /// workspace for the first.
    #[test]
    fn a_call_ran_in_the_worktree_an_assigned_cd_a_pushd_or_git_s_c_names() {
        let dir = repository("operation-ran-in-shapes");
        let base = Tree::new("operation-ran-in-shapes-linked").unwrap();
        let linked = base.join("t1");
        worktree(&dir, &linked);
        std::fs::create_dir(linked.join("sub")).unwrap();
        let top = linked.canonicalize().unwrap();
        let workspace = dir.canonicalize().unwrap();
        let ran_in = |line: &str| Walk::of(&dir, Some(&Line::read(line))).ran_in(&dir);
        let at = linked.display();
        assert_eq!(ran_in(&format!("W={at} && cd \"$W\" && touch x")), top);
        assert_eq!(ran_in(&format!("W={at}; cd $W/sub && touch x")), top);
        // #2995: a `cd` into a file joined by `;` fails, and the line may
        // run where it started as well, so it is held there; one into a
        // directory it can enter does not fail (#3257).
        assert_eq!(ran_in(&format!("W={at}; cd $W/.git; touch x")), workspace);
        assert_eq!(ran_in(&format!("W={at}; cd $W/sub; touch x")), top);
        let exported = format!("export A=1 W={at} && cd \"${{W}}\" && touch x");
        assert_eq!(ran_in(&exported), top);
        assert_eq!(ran_in("cd \"$UNSET_BY_THE_LINE\" && touch x"), workspace);
        assert_eq!(ran_in(&format!("W={at} cd \"$W\" && touch x")), workspace);
        assert_eq!(ran_in(&format!("pushd {at}/sub && touch x")), top);
        assert_eq!(ran_in(&format!("pushd {at} && popd && touch x")), workspace);
        assert_eq!(ran_in("popd && touch x"), workspace);
        let flagged = format!("git -C {at} checkout -- x && git -C {at} status");
        assert_eq!(ran_in(&flagged), top);
        let assigned = format!("export W={at} && git -C \"$W\" checkout -- x");
        assert_eq!(ran_in(&assigned), top);
        assert_eq!(ran_in(&format!("cd {at} && git -C sub status")), top);
        // A `-C` moves its command, not the shell: a line whose other
        // commands run where the shell stands is held there.
        let later = format!("touch x && git -C {at} status");
        assert_eq!(ran_in(&later), workspace);
        let beside = format!("git -C {at} status && git checkout -- x");
        assert_eq!(ran_in(&beside), workspace);
        let after = format!("git -C {at} status && touch x");
        assert_eq!(ran_in(&after), workspace);
        // A change the line cannot read leaves it where it stood.
        assert_eq!(ran_in(&format!("cd {at} && cd \"$UNSET\" && touch x")), top);
        assert_eq!(ran_in(&format!("cd {at} && popd && touch x")), top);
        assert_eq!(ran_in(&format!("pushd -n {at} && touch x")), workspace);
    }
}
