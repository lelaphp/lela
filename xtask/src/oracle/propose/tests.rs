use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::*;
use crate::oracle::stand_in::macho;
use crate::reading::Store;
use crate::scratch::Tree;

/// The keg before the bump, and the one after it: another formula, a
/// revision of its version, and a closure that moved.
const BEFORE: &str = "Cellar/php/9.1.1";
const AFTER: &str = "Cellar/php@9.1/9.1.2_1";

/// What `php -n -i` prints of each keg's configure line.
const CONFIGURED_BEFORE: &str = "'./configure'  '--prefix=/opt/homebrew/Cellar/php/9.1.1' \
     '--with-pear=/opt/homebrew/Cellar/php/9.1.1/share/php/pear' '--enable-bcmath' \
     'PKG_CONFIG_PATH=/opt/homebrew/opt/zip/lib/pkgconfig'";
const CONFIGURED_AFTER: &str = "'./configure'  '--prefix=/opt/homebrew/Cellar/php@9.1/9.1.2_1' \
     '--with-pear=/opt/homebrew/Cellar/php@9.1/9.1.2_1/share/php@9.1/pear' '--enable-bcmath' \
     'PKG_CONFIG_PATH=/opt/homebrew/opt/zip/lib/pkgconfig:/opt/homebrew/opt/zstd/lib/pkgconfig' \
     'CFLAGS=-O2 -g' \"LDFLAGS=say \\\"hi\\\"\"";

fn git(dir: &Path, args: &[&str]) -> String {
    let named = ["-c", "user.name=t", "-c", "user.email=t@t.invalid"];
    let args: Vec<&str> = named.iter().chain(args).copied().collect();
    crate::git::elsewhere(dir, &args).unwrap()
}

fn link(at: &Path, to: &str) {
    fs::create_dir_all(at.parent().unwrap()).unwrap();
    fs::remove_file(at).ok();
    symlink(to, at).unwrap();
}

/// One keg of the prefix: its binaries, php a shell program that prints its
/// configure line and the hosts Mach-O files that load `loads`, and its
/// receipt naming php's `version` and the runtime kegs `named`, each at
/// the version it names.
fn keg(dir: &Path, version: &str, configured: &str, loads: &[&Path], named: &[(&str, &str)]) {
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::create_dir_all(dir.join("sbin")).unwrap();
    let php = dir.join("bin/php");
    let script = format!(
        "#!/bin/sh\necho 'PHP Version => {version}'\ncat <<'EOF'\nConfigure Command =>  {configured}\nEOF\n"
    );
    fs::write(&php, script).unwrap();
    fs::set_permissions(&php, fs::Permissions::from_mode(0o755)).unwrap();
    let loads: Vec<&str> = loads.iter().map(|p| p.to_str().unwrap()).collect();
    for host in ["bin/php-cgi", "sbin/php-fpm"] {
        fs::write(dir.join(host), macho(&loads)).unwrap();
    }
    fs::write(dir.join("bin/phpdbg"), macho(&[])).unwrap();
    let named: Vec<String> = (named.iter())
        .map(|(n, v)| format!("{{\"full_name\":\"{n}\",\"pkg_version\":\"{v}\"}}"))
        .collect();
    let receipt = format!(
        "{{\"source\":{{\"versions\":{{\"stable\":\"{version}\"}}}},\"runtime_dependencies\":[{}]}}",
        named.join(",")
    );
    fs::write(dir.join("INSTALL_RECEIPT.json"), receipt).unwrap();
}

/// The machine of a bump, laid down in a scratch tree: a Homebrew prefix
/// holding both kegs and every keg they load, each `brew pin`ned; php-src
/// checkouts at both tags; and a workspace for the tracked files.
struct Machine {
    prefix: PathBuf,
    src: PathBuf,
    workspace: PathBuf,
}

impl Machine {
    fn lay(tree: &Tree) -> Machine {
        let prefix = tree.join("brew");
        let lib = |keg: &str, version: &str, file: &str, loads: &[&str]| {
            let dir = prefix.join("Cellar").join(keg).join(version);
            fs::create_dir_all(dir.join("lib")).unwrap();
            fs::write(dir.join(file), macho(loads)).unwrap();
            dir.join(file)
        };
        let zstd = lib("zstd", "1.5.7_1", "lib/libzstd.dylib", &[]);
        let zip_before = lib("zip", "1.0", "lib/libzip.dylib", &[]);
        let zip_after = lib("zip", "1.1", "lib/libzip.dylib", &[zstd.to_str().unwrap()]);
        let icu_before = lib("icu4c@78", "78.1", "lib/libicuuc.dylib", &[]);
        let icu_after = lib("icu4c@79", "79.1", "lib/libicuuc.dylib", &[]);
        for version in ["2026-01-01", "2026-02-01"] {
            lib("ca-certificates", version, "lib/cert.pem", &[]);
        }
        // The receipt names zip at a revision the Cellar does not hold, as
        // the pinned keg's receipt names openldap beside the older one it loads.
        let receipt = [
            ("zip", "1.0_1"),
            ("icu4c@78", "78.1"),
            ("ca-certificates", "2026-01-01"),
        ];
        keg(
            &prefix.join(BEFORE),
            "9.1.1",
            CONFIGURED_BEFORE,
            &[&zip_before, &icu_before],
            &receipt,
        );
        let receipt = [
            ("zip", "1.1"),
            ("zstd", "1.5.7_1"),
            ("icu4c@79", "79.1"),
            ("ca-certificates", "2026-02-01"),
        ];
        keg(
            &prefix.join(AFTER),
            "9.1.2",
            CONFIGURED_AFTER,
            &[&zip_after, &icu_after],
            &receipt,
        );
        let opt = [
            ("php", BEFORE),
            ("php@9.1", AFTER),
            ("zip", "Cellar/zip/1.1"),
            ("zstd", "Cellar/zstd/1.5.7_1"),
            ("icu4c@78", "Cellar/icu4c@78/78.1"),
            ("icu4c@79", "Cellar/icu4c@79/79.1"),
        ];
        for (name, at) in opt {
            link(&prefix.join("opt").join(name), &format!("../{at}"));
            let pinned = prefix.join("var/homebrew/pinned").join(name);
            link(&pinned, &format!("../../../{at}"));
        }
        let machine = Machine {
            src: tree.join("src"),
            workspace: tree.join("workspace"),
            prefix,
        };
        machine.certificates("2026-01-01");
        machine.checkouts();
        machine
    }

    /// `ca-certificates`, which nothing loads and the receipt names, at
    /// `version`, as Homebrew's `opt` link and the pin's link name it.
    fn certificates(&self, version: &str) {
        let at = format!("Cellar/ca-certificates/{version}");
        link(
            &self.prefix.join("opt/ca-certificates"),
            &format!("../{at}"),
        );
        let pinned = self.prefix.join("var/homebrew/pinned/ca-certificates");
        link(&pinned, &format!("../../../{at}"));
    }

    /// php-src at `php-9.1.1`, and a clone of it at `php-9.1.2`, which
    /// adds a test and moves one ini file.
    fn checkouts(&self) {
        let before = self.src.join("php-src-9.1.1");
        fs::create_dir_all(before.join("tests")).unwrap();
        git(&before, &["init", "-q"]);
        for (file, text) in [
            ("php.ini-development", "dev 1\n"),
            ("php.ini-production", "prod 1\n"),
            ("tests/a.phpt", ""),
            ("tests/b.phpt", ""),
        ] {
            fs::write(before.join(file), text).unwrap();
        }
        git(&before, &["add", "-A"]);
        git(&before, &["commit", "-qm", "9.1.1"]);
        git(&before, &["tag", "php-9.1.1"]);
        fs::write(before.join("php.ini-development"), "dev 2\n").unwrap();
        fs::write(before.join("tests/c.phpt"), "").unwrap();
        git(&before, &["add", "-A"]);
        git(&before, &["commit", "-qm", "9.1.2"]);
        git(&before, &["tag", "php-9.1.2"]);
        git(&before, &["checkout", "-q", "php-9.1.1"]);
        let after = self.src.join("php-src-9.1.2");
        let (from, to) = (before.to_str().unwrap(), after.to_str().unwrap());
        git(&self.src, &["clone", "-q", from, to]);
        git(&after, &["checkout", "-q", "php-9.1.2"]);
    }

    /// The facts the fixture's `{name}` stand for, read off the keg at
    /// `keg` and the checkout at `tag`, the closure's digest the record's
    /// before the bump, which the variant's build line keeps.
    fn facts(&self, keg: &str, tag: &str, date: &str) -> BTreeMap<String, String> {
        let mut facts = BTreeMap::new();
        facts.insert("prefix".to_owned(), self.prefix.display().to_string());
        facts.insert("src".to_owned(), self.src.display().to_string());
        facts.insert("date".to_owned(), date.to_owned());
        for binary in ["bin/php", "bin/php-cgi", "sbin/php-fpm", "bin/phpdbg"] {
            let bytes = fs::read(self.prefix.join(keg).join(binary)).unwrap();
            let name = binary.rsplit('/').next().unwrap();
            facts.insert(format!("md5:{name}"), format!("{:x}", md5::compute(bytes)));
        }
        let tree = self.src.join("php-src-9.1.1");
        let commit = git(&tree, &["rev-parse", &format!("{tag}^{{commit}}")]);
        let commit = commit.trim().to_owned();
        facts.insert("short".to_owned(), commit[..8].to_owned());
        facts.insert("commit".to_owned(), commit.clone());
        for ini in ["php.ini-development", "php.ini-production"] {
            let bytes = git(&tree, &["show", &format!("{commit}:{ini}")]);
            let digest = Sha256::of(bytes.as_bytes()).to_string();
            facts.insert(format!("ini:{ini}"), digest["sha256:".len()..].to_owned());
        }
        let recorded = "ca-certificates = \"2026-01-01\"\n\"icu4c@78\" = \"78.1\"\nzip = \"1.0\"\n";
        let digest = toml::from_str::<Closure>(recorded)
            .unwrap()
            .digest()
            .to_string();
        facts.insert("closure".to_owned(), digest);
        facts
    }

    /// The tracked files of the fixture's `side`, before or after, each
    /// `{name}` filled from `facts`.
    fn files(side: &str, facts: &BTreeMap<String, String>) -> Vec<(&'static str, String)> {
        let dir = crate::package_dir()
            .join("fixtures/oracle/propose")
            .join(side);
        let filled = |file: &str| {
            let mut text = fs::read_to_string(dir.join(file)).unwrap();
            for (name, value) in facts {
                text = text.replace(&format!("{{{name}}}"), value);
            }
            text
        };
        let tracked = [PIN, CONFIGURE, INPUTS, DOCKERFILE, NOTICE].map(|file| (file, file));
        // The settings stand under `.claude/` in the workspace, and under
        // a name of their own in the fixture, which holds no `.claude/`.
        (tracked
            .into_iter()
            .chain([(SETTINGS, "claude-settings.json")]))
        .map(|(file, fixture)| (file, filled(fixture)))
        .collect()
    }

    fn workspace(&self, files: &[(&str, String)]) {
        for (file, text) in files {
            let at = self.workspace.join(file);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            fs::write(at, text).unwrap();
        }
    }
}

/// Every path under `dir` with its bytes and its modification time, a
/// link by its target.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, Option<SystemTime>)> {
    let mut found = BTreeMap::new();
    let mut queue = vec![dir.to_path_buf()];
    while let Some(at) = queue.pop() {
        let meta = fs::symlink_metadata(&at).unwrap();
        let held = if meta.is_symlink() {
            let target = fs::read_link(&at).unwrap();
            (target.into_os_string().into_encoded_bytes(), None)
        } else if meta.is_dir() {
            queue.extend(
                fs::read_dir(&at)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
            (Vec::new(), None)
        } else {
            (fs::read(&at).unwrap(), meta.modified().ok())
        };
        found.insert(at, held);
    }
    found
}

/// The bump from a keg to the next. Pointed at the pinned keg and tree,
/// the diff is empty (box 1), and the notes still print: the receipt that
/// names zip at a version the Cellar does not hold among them. Pointed at
/// the next keg, poured from another formula at a revision of its version
/// and loading another closure, the diff names every line d2eb1f516 and
/// 6a054ac09 changed at the last bump: `[oracle]`'s version, path and md5,
/// `[closure]`'s rows and date, `[source]`'s tag, commit, path and count,
/// `TEST_PHP_EXECUTABLE`, the three `[sapi]` hosts and the configure
/// line's md5 and arguments; beside them `[oracle].formula`,
/// `dist/inputs.toml`, the Dockerfile's version, `NOTICE`'s tag and commit,
/// nothing else on their line moved, and the settings' tree
/// (box 2). `icu4c@78`, which the new keg loads at its next major and its
/// receipt no longer names, leaves the closure with one note, and
/// `icu4c@79` takes its place. The variant's build line and `ops.toml`
/// are left for their rebuild, and a note names each. `git apply` takes
/// the diff, the files it writes are the fixture's after the bump to the
/// byte, and the pin they make takes its formula and closure checks with
/// no defect but the variant's rebuild. Neither run writes a file anywhere
/// in the machine, nor a reading (box 3).
#[test]
fn a_bump_proposes_every_line_the_pin_states_of_the_keg_and_writes_nothing() {
    let tree = Tree::new("propose").unwrap();
    let machine = Machine::lay(&tree);
    let facts = machine.facts(BEFORE, "php-9.1.1", "");
    let before = Machine::files("before", &facts);
    machine.workspace(&before);
    machine.workspace(&[("ops.toml", "php_tag = \"php-9.1.1\"\n".to_owned())]);
    let ws = &machine.workspace;
    let held = snapshot(&tree);

    let pinned = proposed(ws, None, None, "2026-03-04").unwrap();
    assert_eq!(pinned.diff, "", "{:?}", pinned.notes);
    let notes = pinned.notes.join("\n");
    for said in [
        "the bump writes nothing",
        "zip: the keg's receipt names 1.0_1, and the proposed [closure] records 1.0",
    ] {
        assert!(notes.contains(said), "{notes}");
    }
    assert!(!notes.contains("ops.toml"), "{notes}");
    assert_eq!(snapshot(&tree), held, "the pinned proposal wrote a file");

    machine.certificates("2026-02-01");
    let held = snapshot(&tree);
    let opt = machine.prefix.join("opt/php@9.1");
    let bump = proposed(ws, Some(&opt), None, "2026-03-04").unwrap();
    assert_eq!(snapshot(&tree), held, "the bump's proposal wrote a file");
    assert!(
        !Store::of_workspace(ws).root().exists(),
        "a reading was written"
    );

    let notes = bump.notes.join("\n");
    for said in [
        "[variant.zend-test] pins builds for darwin-arm64, linux-x86_64, each made from the tag the diff replaces",
        "icu4c@78 78.1 of the pin's [closure] is loaded by none of the binaries and named by no receipt, so the diff removes its row",
        "ops.toml names php-9.1.1 and is regenerated from the new tree",
        "the proposed pin carries a defect: [variant.zend-test.darwin-arm64] was made against the closure",
    ] {
        assert!(notes.contains(said), "{notes}");
    }
    assert_eq!(
        notes.matches("of the pin's [closure]").count(),
        1,
        "{notes}"
    );
    for unsaid in ["the keg check", "receipt names", "func-infos"] {
        assert!(!notes.contains(unsaid), "{notes}");
    }

    let patch = tree.join("bump.diff");
    fs::write(&patch, &bump.diff).unwrap();
    git(ws, &["init", "-q"]);
    git(ws, &["apply", patch.to_str().unwrap()]);
    let moved = machine.facts(AFTER, "php-9.1.2", "2026-03-04");
    let (was, now) = (&facts["short"], &moved["short"]);
    let tail = ": data extracted from it (ops.toml), tables transcribed\n";
    let renamed = format!("-php-9.1.1 ({was}){tail}+php-9.1.2 ({now}){tail}");
    assert!(bump.diff.contains(&renamed), "{}", bump.diff);
    let after = Machine::files("after", &moved);
    for (file, text) in &after {
        let written = fs::read_to_string(ws.join(file)).unwrap();
        assert_eq!(&written, text, "{file}\n{}", bump.diff);
    }
    let pin = Pin::parse(&after[0].1).unwrap();
    let [defect] = pin.defects() else {
        panic!("{:?}", pin.defects());
    };
    let rebuild = "[variant.zend-test.darwin-arm64] was made against the closure";
    assert!(defect.starts_with(rebuild), "{defect}");
    assert_eq!(pin.oracle.formula.to_string(), "php@9.1");
    assert_eq!(pin.oracle.bottle(), "9.1.2_1");
    let binaries: Vec<&Path> = pin.binaries().into_iter().map(|(_, path)| path).collect();
    pin.closure.as_ref().unwrap().held(&binaries).unwrap();
}

/// A row of the record no binary loads and no receipt names, whose keg is
/// not installed at all, is proposed as a removal with its note and no
/// refusal, and the date the closure was frozen on stands, since no
/// version was read again. Settings that name a tree other than
/// `[source].path` are left as they stand, with a note.
#[test]
fn a_row_nothing_loads_or_names_is_removed_with_a_note_installed_or_not() {
    let tree = Tree::new("propose-gone").unwrap();
    let machine = Machine::lay(&tree);
    let mut files = Machine::files("before", &machine.facts(BEFORE, "php-9.1.1", ""));
    let row = "\"gone@1\" = \"1.0\"\n";
    files[0].1 = files[0]
        .1
        .replace("zip = \"1.0\"\n", &format!("zip = \"1.0\"\n{row}"));
    let settings = files
        .iter_mut()
        .find(|(file, _)| *file == SETTINGS)
        .unwrap();
    settings.1 = settings.1.replace("/php-src-9.1.1", "/elsewhere");
    machine.workspace(&files);

    let proposal = proposed(&machine.workspace, None, None, "2026-03-04").unwrap();
    let removed: Vec<&str> = (proposal.diff.lines())
        .filter(|line| {
            line.starts_with(['-', '+']) && !line.starts_with("---") && !line.starts_with("+++")
        })
        .collect();
    assert_eq!(
        removed,
        [format!("-{}", row.trim_end())],
        "{}",
        proposal.diff
    );
    let notes = proposal.notes.join("\n");
    for said in [
        "gone@1 1.0 of the pin's [closure] is loaded by none of the binaries and named by no receipt, so the diff removes its row: a person who keeps it leaves that hunk out",
        "which is not [source].path, and the diff leaves it as it stands",
    ] {
        assert!(notes.contains(said), "{notes}");
    }
}

#[test]
fn a_keg_whose_bottle_is_not_its_receipts_version_is_refused() {
    let tree = Tree::new("propose-bottle").unwrap();
    let machine = Machine::lay(&tree);
    machine.workspace(&Machine::files(
        "before",
        &machine.facts(BEFORE, "php-9.1.1", ""),
    ));
    let receipt = machine.prefix.join(AFTER).join("INSTALL_RECEIPT.json");
    let text = fs::read_to_string(&receipt)
        .unwrap()
        .replace("9.1.2", "9.1.3");
    fs::write(&receipt, text).unwrap();
    let keg = machine.prefix.join(AFTER);
    let err = proposed(&machine.workspace, Some(&keg), None, "").unwrap_err();
    let err = err.to_string();
    assert!(
        err.ends_with("holds php 9.1.3, and 9.1.2_1 is no bottle of it"),
        "{err}"
    );
}

#[test]
fn a_checkout_that_is_not_at_the_tag_is_refused() {
    let tree = Tree::new("propose-tag").unwrap();
    let machine = Machine::lay(&tree);
    machine.workspace(&Machine::files(
        "before",
        &machine.facts(BEFORE, "php-9.1.1", ""),
    ));
    let (keg, source) = (
        machine.prefix.join(AFTER),
        machine.src.join("php-src-9.1.1"),
    );
    let err = proposed(&machine.workspace, Some(&keg), Some(&source), "").unwrap_err();
    let err = err.to_string();
    assert!(err.ends_with("check the tag out"), "{err}");
}

#[test]
fn a_configure_command_splits_as_shlex_splits_it() {
    let split = words(r#" './configure'  '--a=b c' "d=\"e\" \x" f\ g '' h"#).unwrap();
    assert_eq!(
        split,
        ["./configure", "--a=b c", r#"d="e" \x"#, "f g", "", "h"]
    );
    for open in ["'a", "\"a", "a\\"] {
        assert!(words(open).is_err(), "{open}");
    }
}

#[test]
fn a_value_is_replaced_where_it_stands_and_a_string_s_lines_are_no_header() {
    let mut text = Text::of(
        "[a]\nx = \"1\"    # kept\n\"k.y\" = { p = \"#\", q = [1] } # too\nz = 2\n\
         m = '''\n[b]\nx = 0\n'''\n[b]\nx = 3\n",
    );
    text.set("a", "x", "\"2\"").unwrap();
    text.set("a", "k.y", "{ p = \"q\" }").unwrap();
    text.set("a", "z", "4").unwrap();
    text.set("b", "x", "5").unwrap();
    assert_eq!(
        text.text(),
        "[a]\nx = \"2\"    # kept\n\"k.y\" = { p = \"q\" } # too\nz = 4\n\
         m = '''\n[b]\nx = 0\n'''\n[b]\nx = 5\n"
    );
    assert!(text.set("a", "w", "1").is_err());
}

/// A diff of several hunks, one at each end of the file and the last
/// leaving the file with no newline, is one `git apply` takes.
#[test]
fn a_unified_diff_applies() {
    let tree = Tree::new("propose-diff").unwrap();
    let lines: Vec<String> = (1..=30).map(|n| format!("line {n}\n")).collect();
    let was = lines.concat();
    let mut now = lines;
    now.insert(0, "first\n".to_owned());
    now[3] = "three\n".to_owned();
    now.remove(15);
    now[29] = "end".to_owned();
    let now = now.concat();
    fs::write(tree.join("f.txt"), &was).unwrap();
    let diff = unified("f.txt", &was, &now);
    assert_eq!(diff.matches("\n@@ ").count(), 3, "{diff}");
    fs::write(tree.join("f.diff"), &diff).unwrap();
    git(&tree, &["init", "-q"]);
    git(&tree, &["apply", "f.diff"]);
    assert_eq!(fs::read_to_string(tree.join("f.txt")).unwrap(), now);
    assert_eq!(unified("f.txt", &was, &was), "");
}
