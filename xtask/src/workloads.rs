//! The workload programs (measurement protocol §11): the directory they
//! live in, named once, and their declaration beside it,
//! `docs/perf/workloads.toml` ([`file`]), decoded once into a
//! [`Declaration`] (#4335, #4344). The declaration gives each program its
//! [`Role`], the runs it is measured by and the status each ends on, and
//! `bench`, `dispatch-census` and `trace-dump` reach a program only
//! through the [`Run`]s it yields, each with its arguments. It also
//! declares the bench's pages ([`Pages`]), served from a rig kept off the
//! repository, which `dispatch-census` counts (#4364, #4367). `budget` holds
//! the programs under `[budget] workloads` apart from `docs` where that
//! row stands (#4341), and the declaration under `docs`. makala's bench
//! and soak serve, each as a request, the runs of every role
//! [`Role::served`] serves, a native's none (#4366), each run that takes
//! no arguments, since a served request has no `$argv`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::oracle::Ended;

/// The directory of the workload programs, relative to the workspace root.
pub(crate) const DIR: &str = "docs/perf/workloads";

/// [`DIR`] under `root`.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

/// The declaration under `root`: `<DIR>.toml`, beside the directory.
pub(crate) fn file(root: &Path) -> PathBuf {
    root.join(format!("{DIR}.toml"))
}

/// What a workload is measured for, as #4335 ruled the roles; which of
/// them votes in `IN_THE_LOOP`'s criterion is [`Role::votes`]'s alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Role {
    /// One of the seven kernels the tier's gates are measured on.
    Kernel,
    /// A program the bench's bars run beside the kernels.
    Bench,
    /// A program written to the opline mix of real pages.
    Shape,
    /// A program that holds one mechanism of the loop to a count or a bar.
    Probe,
    /// A program whose time is spent in an extension's natives.
    Native,
}

impl Role {
    /// The name the declaration spells it by.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Role::Kernel => "kernel",
            Role::Bench => "bench",
            Role::Shape => "shape",
            Role::Probe => "probe",
            Role::Native => "native",
        }
    }

    /// Whether a run of this role votes in `IN_THE_LOOP`'s criterion, as
    /// #4364 amended #4335's ruling: a kernel and the bench do; a probe,
    /// which holds one mechanism by construction and would vote in
    /// whatever it probes, does not, nor a shape, kept for timing alone,
    /// nor a native, whose time is spent outside the loop.
    pub(crate) fn votes(self) -> bool {
        match self {
            Role::Kernel | Role::Bench => true,
            Role::Shape | Role::Probe | Role::Native => false,
        }
    }

    /// Whether a run of this role is served as a request on makala's web
    /// bars and its soak, as #4366 ruled: a native's is not, its time
    /// spent in an extension's natives, and what a served request adds to
    /// it, its startup, its teardown and the response it writes, the
    /// other roles' bars measure already; every other role's is.
    pub(crate) fn served(self) -> bool {
        match self {
            Role::Kernel | Role::Bench | Role::Shape | Role::Probe => true,
            Role::Native => false,
        }
    }
}

/// Where a workload's text is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Text {
    /// Its file, `<DIR>/<stem>.php`, run in its own directory.
    File(PathBuf),
    /// A file of the pinned php-src, by its path in the tree.
    Pinned(PathBuf),
    /// Code the declaration gives whole, run as a witness's snippet is.
    Code(String),
}

/// One run a workload is measured by: its name and the arguments after
/// the program's path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub(crate) name: String,
    pub(crate) args: Vec<String>,
}

/// The runs a workload is measured by.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Runs {
    /// One, with no arguments.
    Bare,
    /// One per profile, never none.
    Profiled(Vec<Profile>),
}

/// One declared workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workload {
    /// What names it: its file's stem, and its runs' scopes.
    pub(crate) stem: String,
    /// The ticket that first read it.
    pub(crate) first_read: u32,
    /// What it is, for a person.
    pub(crate) about: String,
    pub(crate) role: Role,
    /// How each of its runs ends.
    pub(crate) ends: Ended,
    text: Text,
    runs: Runs,
}

impl Workload {
    /// Where its text is.
    pub(crate) fn text(&self) -> &Text {
        &self.text
    }

    /// Its runs' profiles, none where it runs bare.
    pub(crate) fn profiles(&self) -> &[Profile] {
        match &self.runs {
            Runs::Bare => &[],
            Runs::Profiled(profiles) => profiles,
        }
    }

    /// Its runs, one per profile or one bare.
    pub(crate) fn runs(&self) -> Vec<Run<'_>> {
        match &self.runs {
            Runs::Bare => vec![Run {
                workload: self,
                profile: None,
            }],
            Runs::Profiled(profiles) => (profiles.iter())
                .map(|profile| Run {
                    workload: self,
                    profile: Some(profile),
                })
                .collect(),
        }
    }
}

/// One run of a declared workload: the one way a reader reaches a
/// workload's program, with the arguments the declaration gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run<'a> {
    pub(crate) workload: &'a Workload,
    profile: Option<&'a Profile>,
}

impl Run<'_> {
    /// Its name: the workload's stem, and `/<profile>` after it where it
    /// runs under one.
    pub(crate) fn name(&self) -> String {
        match self.profile {
            None => self.workload.stem.clone(),
            Some(profile) => format!("{}/{}", self.workload.stem, profile.name),
        }
    }

    /// Where its program's text is.
    pub(crate) fn text(&self) -> &Text {
        &self.workload.text
    }

    /// Whether it runs under no profile, as a served request does, which
    /// has no `$argv`.
    pub(crate) fn is_bare(&self) -> bool {
        self.profile.is_none()
    }

    /// The arguments after the program's path.
    pub(crate) fn args(&self) -> &[String] {
        self.profile.map_or(&[], |profile| &profile.args)
    }

    /// Its program's file, for a reader that runs files alone, refused
    /// where its text is elsewhere.
    pub(crate) fn file(&self) -> Result<&Path> {
        match &self.workload.text {
            Text::File(path) => Ok(path),
            Text::Pinned(_) | Text::Code(_) => bail!(
                "{} is declared with no file under {DIR}",
                self.workload.stem
            ),
        }
    }
}

/// One `[[workload]]` as the file spells it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    stem: String,
    first_read: u32,
    about: String,
    role: Role,
    #[serde(default)]
    exit: i32,
    #[serde(default)]
    pinned: Option<PathBuf>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    profile: Option<Vec<Profile>>,
}

/// The status a page answers, or its counts are out of the vote.
pub(crate) const ANSWERS: u16 = 200;

/// One page of the bench: what names its runs' scope, and the target of
/// its request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub(crate) name: String,
    /// The request's target, its path and query: `/?p=1`.
    pub(crate) path: String,
}

/// The bench's pages (#4364): requests served by the CLI's built-in
/// server from a copy of the rig, each on a server of its own over a
/// fresh copy of the rig's database, the counted request after `warm`
/// that are not.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pages {
    /// The ticket that first read them.
    pub(crate) first_read: u32,
    /// What they are, for a person.
    pub(crate) about: String,
    pub(crate) role: Role,
    /// The requests of a page served before the one counted.
    pub(crate) warm: u32,
    /// The settings the server runs under, each passed as `-d` after `-n`.
    pub(crate) ini: Vec<String>,
    /// Each page, in the order served.
    pub(crate) page: Vec<Page>,
}

impl Pages {
    /// The pages as the file spells them, refused where they list no page,
    /// name one twice or misspelt, or give one a target that is not a
    /// path an HTTP request line carries.
    fn held(self) -> Result<Pages> {
        if self.page.is_empty() {
            bail!("[pages] lists no page");
        }
        let mut names = BTreeSet::new();
        for page in &self.page {
            let name = spelt(&page.name, "page")?;
            if !names.insert(name.clone()) {
                bail!("[pages] names the page {name:?} twice");
            }
            let carried = |c: char| c.is_ascii_graphic();
            if !page.path.starts_with('/') || !page.path.chars().all(carried) {
                bail!(
                    "the page {name:?} is served at {:?}, which is no path starting with `/` in printable ASCII",
                    page.path
                );
            }
        }
        Ok(self)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    workload: Vec<Row>,
    #[serde(default)]
    pages: Option<Pages>,
}

/// The declaration: every workload, in the file's order, and the bench's
/// pages where it declares them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Declaration {
    pub(crate) workloads: Vec<Workload>,
    pub(crate) pages: Option<Pages>,
}

impl Declaration {
    /// The declaration of the workspace at `root`, held to the directory:
    /// refused where a program under [`DIR`] has no row or a row's file is
    /// not there, so a program is never measured undeclared.
    pub(crate) fn load(root: &Path) -> Result<Declaration> {
        let path = file(root);
        let shown = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let declared = Declaration::parse(&crate::read(&path)?, root)
            .with_context(|| format!("{shown} is no declaration"))?;
        declared
            .held_to_the_directory(root)
            .with_context(|| format!("{shown} is not {DIR}'s"))?;
        Ok(declared)
    }

    /// `text` as a declaration of the programs under `root`'s [`DIR`]:
    /// refused where it holds no workload, names a stem or a workload's
    /// profile twice, spells one with other than lowercase letters,
    /// digits, `_` and `-`, gives a workload both `pinned` and `code`, or
    /// lists its profiles empty, and where its pages are refused
    /// ([`Pages::held`]).
    pub(crate) fn parse(text: &str, root: &Path) -> Result<Declaration> {
        let File { workload, pages } = toml::from_str(text)?;
        if workload.is_empty() {
            bail!("it declares no [[workload]]");
        }
        let mut stems = BTreeSet::new();
        let pages = pages.map(Pages::held).transpose()?;
        let workloads = workload.into_iter().map(|row| {
            let stem = spelt(&row.stem, "stem")?;
            if !stems.insert(stem.clone()) {
                bail!("the stem {stem:?} names two workloads");
            }
            let text = match (row.pinned, row.code) {
                (None, None) => Text::File(dir(root).join(format!("{stem}.php"))),
                (Some(pinned), None) => Text::Pinned(pinned),
                (None, Some(code)) if !code.trim().is_empty() => Text::Code(code),
                (None, Some(_)) => bail!("{stem} gives its code empty"),
                (Some(_), Some(_)) => bail!("{stem} gives both `pinned` and `code`"),
            };
            let runs = match row.profile {
                None => Runs::Bare,
                Some(profiles) if profiles.is_empty() => {
                    bail!("{stem} lists its profiles empty; a bare workload lists none")
                }
                Some(profiles) => {
                    let mut names = BTreeSet::new();
                    for profile in &profiles {
                        let name = spelt(&profile.name, "profile")?;
                        if !names.insert(name.clone()) {
                            bail!("{stem} names the profile {name:?} twice");
                        }
                    }
                    Runs::Profiled(profiles)
                }
            };
            Ok(Workload {
                first_read: row.first_read,
                about: row.about,
                role: row.role,
                ends: Ended::Status(row.exit),
                text,
                runs,
                stem,
            })
        });
        let workloads = workloads.collect::<Result<_>>()?;
        Ok(Declaration { workloads, pages })
    }

    /// Every run, workload by workload in the file's order.
    pub(crate) fn runs(&self) -> impl Iterator<Item = Run<'_>> {
        self.workloads.iter().flat_map(Workload::runs)
    }

    /// The runs of `role`'s workloads, in the file's order.
    pub(crate) fn runs_of(&self, role: Role) -> impl Iterator<Item = Run<'_>> {
        self.runs().filter(move |run| run.workload.role == role)
    }

    /// Refused where a program under `root`'s [`DIR`] has no row or a
    /// row's file is not there, each named.
    fn held_to_the_directory(&self, root: &Path) -> Result<()> {
        let dir = dir(root);
        let mut files = BTreeSet::new();
        for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {DIR}"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "php") {
                files.insert(path);
            }
        }
        let rows: BTreeSet<PathBuf> = (self.workloads.iter())
            .filter_map(|w| match &w.text {
                Text::File(path) => Some(path.clone()),
                Text::Pinned(_) | Text::Code(_) => None,
            })
            .collect();
        let named = |paths: &[&PathBuf]| {
            let names = paths.iter().filter_map(|p| p.file_name());
            crate::reading::joined(names.map(|n| n.to_string_lossy()), ", ")
        };
        let undeclared: Vec<&PathBuf> = files.difference(&rows).collect();
        if let [one] = undeclared[..] {
            bail!("a program has no row: {}", named(&[one]));
        }
        if !undeclared.is_empty() {
            bail!("programs have no row: {}", named(&undeclared));
        }
        let absent: Vec<&PathBuf> = rows.difference(&files).collect();
        if !absent.is_empty() {
            bail!("no file stands for {}", named(&absent));
        }
        Ok(())
    }
}

/// `name` where it is spelt with lowercase letters, digits, `_` and `-`
/// alone, as a reading's scope takes it.
fn spelt(name: &str, what: &str) -> Result<String> {
    let spelt = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
    if name.is_empty() || !name.chars().all(spelt) {
        bail!(
            "the {what} {name:?} is spelt with other than lowercase letters, digits, `_` and `-`"
        );
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECLARED: &str = r#"
[[workload]]
stem = "fib"
first_read = 2740
about = "a kernel"
role = "kernel"

[[workload]]
stem = "modes"
first_read = 2826
about = "a native's"
role = "native"
exit = 255
profile = [
  { name = "small", args = ["small", "1"] },
  { name = "large", args = ["large", "1"] },
]

[[workload]]
stem = "micro_bench"
first_read = 3285
about = "php-src's"
role = "probe"
pinned = "Zend/micro_bench.php"

[[workload]]
stem = "loop-empty"
first_read = 3273
about = "a loop"
role = "probe"
code = "for ($i = 0; $i < 3; $i++) {}\n"

[pages]
first_read = 4329
about = "pages"
role = "bench"
warm = 3
ini = ["opcache.enable=1"]
page = [{ name = "front", path = "/" }, { name = "post", path = "/?p=1" }]
"#;

    /// Each row decodes to its role, where its text is, how it ends, zero
    /// where it states nothing, and its runs: one bare, or one per profile
    /// named `<stem>/<profile>` with that profile's arguments.
    #[test]
    fn each_row_decodes_to_its_role_its_runs_and_its_end() {
        let root = Path::new("/w");
        let declared = Declaration::parse(DECLARED, root).unwrap();
        let runs: Vec<(String, Vec<String>, Role, Ended)> = (declared.runs())
            .map(|r| {
                (
                    r.name(),
                    r.args().to_vec(),
                    r.workload.role,
                    r.workload.ends,
                )
            })
            .collect();
        let run = |name: &str, args: &[&str], role, status| {
            let args = args.iter().map(|s| (*s).to_owned()).collect();
            (name.to_owned(), args, role, Ended::Status(status))
        };
        assert_eq!(
            runs,
            [
                run("fib", &[], Role::Kernel, 0),
                run("modes/small", &["small", "1"], Role::Native, 255),
                run("modes/large", &["large", "1"], Role::Native, 255),
                run("micro_bench", &[], Role::Probe, 0),
                run("loop-empty", &[], Role::Probe, 0),
            ]
        );
        let texts: Vec<&Text> = declared.workloads.iter().map(Workload::text).collect();
        assert_eq!(
            texts,
            [
                &Text::File(PathBuf::from("/w/docs/perf/workloads/fib.php")),
                &Text::File(PathBuf::from("/w/docs/perf/workloads/modes.php")),
                &Text::Pinned(PathBuf::from("Zend/micro_bench.php")),
                &Text::Code("for ($i = 0; $i < 3; $i++) {}\n".to_owned()),
            ]
        );
        let kernels: Vec<String> = declared.runs_of(Role::Kernel).map(|r| r.name()).collect();
        assert_eq!(kernels, ["fib"]);
        assert!(declared.runs().nth(3).unwrap().file().is_err());
        let pages = declared.pages.unwrap();
        assert_eq!((pages.role, pages.warm), (Role::Bench, 3));
        assert_eq!(pages.ini, ["opcache.enable=1"]);
        let served: Vec<(&str, &str)> = (pages.page.iter())
            .map(|p| (p.name.as_str(), p.path.as_str()))
            .collect();
        assert_eq!(served, [("front", "/"), ("post", "/?p=1")]);
    }

    /// Which role votes is decided here alone, as #4364 ruled: a kernel
    /// and the bench vote, and a shape, a probe and a native do not.
    #[test]
    fn a_kernel_and_the_bench_vote_and_no_other_role_does() {
        let roles = [
            Role::Kernel,
            Role::Bench,
            Role::Shape,
            Role::Probe,
            Role::Native,
        ];
        assert_eq!(roles.map(Role::votes), [true, true, false, false, false]);
    }

    /// Which role makala serves on its web bars and soak is decided here
    /// alone, as #4366 ruled: every role but a native.
    #[test]
    fn every_role_but_a_native_is_served_on_the_web_bars() {
        let roles = [
            Role::Kernel,
            Role::Bench,
            Role::Shape,
            Role::Probe,
            Role::Native,
        ];
        assert_eq!(roles.map(Role::served), [true, true, true, true, false]);
    }

    /// A row the file does not spell as a declaration is refused, naming
    /// what is wrong: a field it does not know, a role none of the five,
    /// a stem twice or misspelt, both `pinned` and `code`, profiles
    /// listed empty or one named twice, and pages listed empty, one named
    /// twice or one served at no path.
    #[test]
    fn a_misdeclared_row_is_refused_naming_what_is_wrong() {
        let refused = |rows: &str| {
            let text = format!("[[workload]]\nfirst_read = 1\nabout = \"x\"\n{rows}");
            let why = Declaration::parse(&text, Path::new("/w")).unwrap_err();
            format!("{why:#}")
        };
        let twice = "stem = \"a\"\nrole = \"kernel\"\n[[workload]]\nstem = \"a\"\n\
                     first_read = 1\nabout = \"y\"\nrole = \"bench\"\n";
        let profiled_twice = "stem = \"a\"\nrole = \"native\"\n\
                              profile = [{ name = \"p\", args = [] }, { name = \"p\", args = [] }]\n";
        let both = "stem = \"a\"\nrole = \"probe\"\npinned = \"x.php\"\ncode = \"1;\"\n";
        let cases = [
            (
                "stem = \"a\"\nrole = \"kernel\"\nargs = []\n",
                "unknown field `args`",
            ),
            ("stem = \"a\"\nrole = \"hot\"\n", "unknown variant `hot`"),
            (
                "stem = \"A\"\nrole = \"kernel\"\n",
                "the stem \"A\" is spelt",
            ),
            (both, "gives both `pinned` and `code`"),
            (
                "stem = \"a\"\nrole = \"native\"\nprofile = []\n",
                "lists its profiles empty",
            ),
            (profiled_twice, "names the profile \"p\" twice"),
            (twice, "the stem \"a\" names two workloads"),
        ];
        for (rows, said) in cases {
            let why = refused(rows);
            assert!(why.contains(said), "{rows}: {why}");
        }
        let listing = |pages: &str| {
            let rows = format!(
                "stem = \"a\"\nrole = \"kernel\"\n[pages]\nfirst_read = 1\nabout = \"x\"\n\
                 role = \"bench\"\nwarm = 3\nini = []\npage = [{pages}]\n"
            );
            refused(&rows)
        };
        let misdeclared = [
            ("", "[pages] lists no page"),
            (
                "{ name = \"a\", path = \"/\" }, { name = \"a\", path = \"/b\" }",
                "names the page \"a\" twice",
            ),
            (
                "{ name = \"a\", path = \"b\" }",
                "the page \"a\" is served at \"b\"",
            ),
            (
                "{ name = \"a\", path = \"/ b\" }",
                "the page \"a\" is served at \"/ b\"",
            ),
        ];
        for (pages, said) in misdeclared {
            let why = listing(pages);
            assert!(why.contains(said), "{pages}: {why}");
        }
    }

    /// A program under the directory with no row is refused by name, and a
    /// row whose file is not there: `xtask/fixtures/workloads`' two trees.
    #[test]
    fn a_program_without_a_row_and_a_row_without_its_program_are_refused() {
        let fixtures = crate::package_dir().join("fixtures/workloads");
        let stray = Declaration::load(&fixtures.join("stray")).unwrap_err();
        let stray = format!("{stray:#}");
        assert!(stray.contains("a program has no row: stray.php"), "{stray}");
        let gone = Declaration::load(&fixtures.join("gone")).unwrap_err();
        let gone = format!("{gone:#}");
        assert!(gone.contains("no file stands for gone.php"), "{gone}");
    }

    /// The workspace's declaration holds every program under the directory
    /// and no row without one, each in the role #4335 ruled: the three
    /// programs that take arguments and the six `uri-*` are native, each
    /// of the three run under the profiles its record ran it with; the
    /// seven kernels and `bench_quiet` are the workloads that vote, as
    /// #4364 ruled, and the four bench pages vote beside them.
    #[cfg(tracked)]
    #[test]
    fn the_declaration_is_the_directorys_with_the_ruled_roles() {
        let root = crate::package_dir().join("..");
        let declared = Declaration::load(&root).unwrap();
        let native: BTreeSet<&str> = (declared.workloads.iter())
            .filter(|w| w.role == Role::Native)
            .map(|w| w.stem.as_str())
            .collect();
        let taking = ["fileinfo", "compression", "mbstring_profiles_2831"];
        let mut ruled: BTreeSet<String> = ["rfc", "whatwg"]
            .into_iter()
            .flat_map(|s| ["read-update", "unicode", "validation"].map(|k| format!("uri-{s}-{k}")))
            .collect();
        ruled.extend(taking.map(str::to_owned));
        assert_eq!(native, ruled.iter().map(String::as_str).collect());
        let profiled = |stem: &str| {
            let w = declared.workloads.iter().find(|w| w.stem == stem).unwrap();
            w.profiles().len()
        };
        assert_eq!(taking.map(profiled), [4, 7, 8]);
        let kernels: Vec<String> = declared.runs_of(Role::Kernel).map(|r| r.name()).collect();
        assert_eq!(kernels.len(), 7, "{kernels:?}");
        let voting: Vec<&str> = (declared.workloads.iter())
            .filter(|w| w.role.votes())
            .map(|w| w.stem.as_str())
            .collect();
        assert_eq!(voting.len(), 8, "{voting:?}");
        assert!(voting.contains(&"bench_quiet"), "{voting:?}");
        let pages = declared.pages.unwrap();
        assert!(pages.role.votes());
        let served: Vec<&str> = pages.page.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(served, ["/", "/?p=1", "/?s=hello", "/wp-login.php"]);
    }
}
