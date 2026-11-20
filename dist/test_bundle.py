"""dist/bundle.py, held on stand-in Mach-O files that any machine can hold,
and on an Apple silicon Mac on libraries cc links, with the real otool,
install_name_tool and codesign.

A stand-in Mach-O is the Mach-O magic followed by a JSON object of what it
names. The stand-in otool answers `otool -l` for it with the real tool's
text, each load command a block of a capture in dist/fixtures/otool/ with
its name, path or minimum put in; the stand-in install_name_tool edits the
object as the real one edits the file, and refuses as it does to delete a
run path the file has not; the stand-in codesign marks it signed. Each
capture is the real otool's output on an Apple silicon Mac: `lela.txt` of a
host linking Homebrew's kegs, `libbrotlidec.1.dylib.txt` of a keg's library
with its own install name, an `@rpath` name and a run path,
`libjpeg.8.dylib.txt` of one with an absolute run path, and
`bundled-libbrotlidec.1.dylib.txt` of that library as the staging leaves
it. The parser is held to each capture whole, and the stand-in to the
capture's own lines.
"""

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import bundle
from publish import Refusal

DIST = Path(__file__).resolve().parent
FIXTURES = DIST / "fixtures/otool"
MAGIC = bundle.MAGIC[0]
# The warning the real install_name_tool writes for each file it changes.
WARNING = "warning: changes being made to the file will invalidate the code signature in: {}"


def blocks():
    """The load commands of the brotli capture, the first of each kind."""
    text = (FIXTURES / "libbrotlidec.1.dylib.txt").read_text()
    found = {}
    for block in re.split(r"^Load command \d+\n", text, flags=re.MULTILINE)[1:]:
        found.setdefault(re.search(r"^\s*cmd (\S+)$", block, re.MULTILINE).group(1), block)
    return found


def put(block, key, value):
    """`block` with the value of its `key` line made `value`, any
    `(offset <n>)` after it kept."""
    pattern = rf"^(\s*{key} ).*?((?: \(offset \d+\))?)$"
    return re.sub(pattern, lambda m: m.group(1) + value + m.group(2), block, count=1, flags=re.MULTILINE)


def render(path, macho):
    """`otool -l`'s text for a stand-in's object, from the real blocks."""
    real = blocks()
    commands = [real["LC_SEGMENT_64"]]
    if macho.get("id"):
        commands.append(put(real["LC_ID_DYLIB"], "name", macho["id"]))
    commands.append(put(real["LC_BUILD_VERSION"], "minos", macho["minos"]))
    commands += [put(real["LC_LOAD_DYLIB"], "name", name) for name in macho["names"]]
    commands += [put(real["LC_RPATH"], "path", rpath) for rpath in macho["rpaths"]]
    return f"{path}:\n" + "".join(f"Load command {n}\n{block}" for n, block in enumerate(commands))


def load(path):
    data = Path(path).read_bytes()
    if not data.startswith(MAGIC):
        return None
    return json.loads(data[len(MAGIC):])


def store(path, macho):
    Path(path).write_bytes(MAGIC + json.dumps(macho).encode())


def record(*call):
    with open(os.environ["CALLS"], "a") as log:
        log.write(json.dumps(call) + "\n")


def otool(arguments):
    """`otool -l <file>`, the one call the staging makes."""
    if arguments[:1] != ["-l"] or len(arguments) != 2:
        sys.exit(f"the stand-in otool has no answer for {arguments}")
    macho = load(arguments[1])
    if macho is None:
        print(f"{arguments[1]}: is not an object file")
        return
    sys.stdout.write(render(arguments[1], macho))


def install_name_tool(arguments):
    *options, path = arguments
    record("install_name_tool", *arguments)
    if os.environ.get("IGNORES"):
        return
    macho = load(path)
    while options:
        option = options.pop(0)
        if option == "-id":
            macho["id"] = options.pop(0)
        elif option == "-change":
            old, new = options.pop(0), options.pop(0)
            macho["names"] = [new if name == old else name for name in macho["names"]]
        elif option == "-delete_rpath":
            rpath = options.pop(0)
            if rpath not in macho["rpaths"]:
                sys.exit(f"install_name_tool: no LC_RPATH load command with path: {rpath} found in: {path} "
                         "(for architecture arm64), required for specified option \"-delete_rpath {rpath}\"")
            macho["rpaths"].remove(rpath)
        else:
            sys.exit(f"the stand-in install_name_tool has no option {option}")
    store(path, macho)
    print(f"install_name_tool: {WARNING.format(path)}", file=sys.stderr)


def codesign(arguments):
    record("codesign", *arguments)
    if arguments[:3] != ["--force", "--sign", "-"]:
        sys.exit(f"the stand-in codesign has no answer for {arguments}")
    macho = load(arguments[3])
    macho["signed"] = True
    store(arguments[3], macho)


# Each stand-in is this module, run as the tool its name is.
STAND_IN = f'''#!{sys.executable}
import os, sys
sys.path.insert(0, {str(DIST)!r})
import test_bundle
getattr(test_bundle, os.path.basename(sys.argv[0]))(sys.argv[1:])
'''


class Captures(unittest.TestCase):
    def capture(self, name):
        return bundle.parse(name, (FIXTURES / name).read_text())

    def test_the_parser_reads_each_capture_whole(self):
        host = self.capture("lela.txt")
        self.assertEqual((host.id, host.rpaths, host.minos), (None, (), "15.0"))
        self.assertEqual(len(host.names), 22)
        self.assertEqual(host.names[:2], ("/usr/lib/libSystem.B.dylib", "/opt/homebrew/opt/zstd/lib/libzstd.1.dylib"))
        self.assertIn("/opt/homebrew/opt/icu4c@79/lib/libicuuc.79.dylib", host.names)
        self.assertEqual(self.capture("libbrotlidec.1.dylib.txt"), bundle.MachO(
            id="/opt/homebrew/opt/brotli/lib/libbrotlidec.1.dylib",
            names=("/usr/lib/libSystem.B.dylib", "@rpath/libbrotlicommon.1.dylib"),
            rpaths=("@loader_path/../lib",), minos="15.0",
        ))
        self.assertEqual(self.capture("libjpeg.8.dylib.txt").rpaths, ("/opt/homebrew/Cellar/jpeg-turbo/3.2.1/lib",))
        self.assertEqual(self.capture("bundled-libbrotlidec.1.dylib.txt"), bundle.MachO(
            id="@rpath/libbrotlidec.1.dylib",
            names=("/usr/lib/libSystem.B.dylib", "@loader_path/libbrotlicommon.1.dylib"),
            rpaths=(), minos="15.0",
        ))

    def test_the_stand_in_answers_in_the_captures_lines(self):
        capture = (FIXTURES / "libbrotlidec.1.dylib.txt").read_text()
        for cmd, block in blocks().items():
            with self.subTest(cmd=cmd):
                self.assertIn(block, capture)
        real = blocks()
        self.assertEqual(put(real["LC_LOAD_DYLIB"], "name", "/usr/lib/libSystem.B.dylib"), real["LC_LOAD_DYLIB"])
        self.assertEqual(put(real["LC_RPATH"], "path", "@loader_path/../lib"), real["LC_RPATH"])
        self.assertEqual(put(real["LC_BUILD_VERSION"], "minos", "15.0"), real["LC_BUILD_VERSION"])
        parsed = self.capture("libbrotlidec.1.dylib.txt")
        macho = {"id": parsed.id, "names": list(parsed.names), "rpaths": list(parsed.rpaths), "minos": parsed.minos}
        self.assertEqual(bundle.parse("x", render("x", macho)), parsed)


class Tree(unittest.TestCase):
    """A Homebrew prefix of stand-in kegs laid as the real ones are, each
    library's real file versioned and reached through the keg's `opt` link
    and the library's own, and a stage with the hosts linking them."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name).resolve()
        self.prefix = self.scratch / "homebrew"
        self.stage = self.scratch / "lela-1.2.0-darwin-arm64"
        self.bin = self.scratch / "bin"
        self.bin.mkdir()
        for tool in ("otool", "install_name_tool", "codesign"):
            (self.bin / tool).write_text(STAND_IN)
            (self.bin / tool).chmod(0o755)
        self.calls = self.scratch / "calls"
        saved = dict(os.environ)
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(saved)))
        os.environ["PATH"] = f"{self.bin}{os.pathsep}{os.environ['PATH']}"
        os.environ["CALLS"] = str(self.calls)
        p = self.prefix
        self.keg("brotli", "1.2.1", "LICENSE", {
            "libbrotlidec.1.2.1.dylib": ("libbrotlidec.1.dylib", {
                "id": f"{p}/opt/brotli/lib/libbrotlidec.1.dylib",
                "names": ["/usr/lib/libSystem.B.dylib", "@rpath/libbrotlicommon.1.dylib"],
                "rpaths": ["@loader_path/../lib"], "minos": "15.0",
            }),
            "libbrotlicommon.1.2.1.dylib": ("libbrotlicommon.1.dylib", {
                "id": f"{p}/opt/brotli/lib/libbrotlicommon.1.dylib",
                "names": ["/usr/lib/libSystem.B.dylib"], "rpaths": [], "minos": "15.0",
            }),
        })
        self.keg("jpeg-turbo", "3.2.1", "LICENSE.md", {
            "libjpeg.8.3.2.dylib": ("libjpeg.8.dylib", {
                "id": f"{p}/opt/jpeg-turbo/lib/libjpeg.8.dylib", "names": ["/usr/lib/libSystem.B.dylib"],
                "rpaths": [f"{p}/Cellar/jpeg-turbo/3.2.1/lib"], "minos": "15.10",
            }),
        })
        self.keg("icu4c@79", "79.1", "LICENSE", {
            "libicuuc.79.1.dylib": ("libicuuc.79.dylib", {
                "id": f"{p}/opt/icu4c@79/lib/libicuuc.79.dylib",
                "names": ["@loader_path/libicudata.79.dylib", "/usr/lib/libc++.1.dylib"],
                "rpaths": [], "minos": "15.2",
            }),
            "libicudata.79.1.dylib": ("libicudata.79.dylib", {
                "id": f"{p}/opt/icu4c@79/lib/libicudata.79.dylib", "names": [], "rpaths": [], "minos": "15.0",
            }),
        })
        # libssl names libcrypto by its Cellar path, the host by its opt one.
        self.keg("openssl@4", "4.1.0", "LICENSE.txt", {
            "libssl.4.dylib": (None, {
                "id": f"{p}/opt/openssl@4/lib/libssl.4.dylib",
                "names": [f"{p}/Cellar/openssl@4/4.1.0/lib/libcrypto.4.dylib", "/usr/lib/libSystem.B.dylib"],
                "rpaths": [], "minos": "15.0",
            }),
            "libcrypto.4.dylib": (None, {
                "id": f"{p}/opt/openssl@4/lib/libcrypto.4.dylib", "names": [], "rpaths": [], "minos": "15.0",
            }),
        })
        links = [
            "/usr/lib/libSystem.B.dylib", f"{p}/opt/brotli/lib/libbrotlidec.1.dylib",
            f"{p}/opt/jpeg-turbo/lib/libjpeg.8.dylib", f"{p}/opt/icu4c@79/lib/libicuuc.79.dylib",
            f"{p}/opt/openssl@4/lib/libssl.4.dylib", f"{p}/opt/openssl@4/lib/libcrypto.4.dylib",
        ]
        self.hosts = [self.stage / "bin/lela", self.stage / "sbin/lela-fpm"]
        for host, names in zip(self.hosts, (links, links[:2])):
            host.parent.mkdir(parents=True, exist_ok=True)
            store(host, {"names": names, "rpaths": [], "minos": "15.0"})

    def keg(self, keg, version, licence, files):
        lib = self.prefix / "Cellar" / keg / version / "lib"
        lib.mkdir(parents=True)
        (lib.parent / licence).write_text(f"the licence of {keg}\n")
        (lib.parent / "README.md").write_text("no licence\n")
        (self.prefix / "opt").mkdir(exist_ok=True)
        (self.prefix / "opt" / keg).symlink_to(f"../Cellar/{keg}/{version}")
        for real, (link, macho) in files.items():
            store(lib / real, macho)
            if link:
                (lib / link).symlink_to(real)

    def logged(self):
        if not self.calls.exists():
            return []
        return [json.loads(line) for line in self.calls.read_text().splitlines()]

    def refused(self, said):
        with self.assertRaises(Refusal) as refusal:
            bundle.bundle(self.stage, self.hosts)
        self.assertIn(said, str(refusal.exception))
        self.assertFalse((self.stage / "lib").exists())


class Closure(Tree):
    def test_the_closure_is_every_library_the_hosts_load_through_every_other(self):
        self.assertEqual([(lib.name, lib.keg, lib.version) for lib in bundle.closure(self.hosts)], [
            ("libbrotlicommon.1.dylib", "brotli", "1.2.1"),
            ("libbrotlidec.1.dylib", "brotli", "1.2.1"),
            ("libcrypto.4.dylib", "openssl@4", "4.1.0"),
            ("libicudata.79.dylib", "icu4c@79", "79.1"),
            ("libicuuc.79.dylib", "icu4c@79", "79.1"),
            ("libjpeg.8.dylib", "jpeg-turbo", "3.2.1"),
            ("libssl.4.dylib", "openssl@4", "4.1.0"),
        ])
        self.assertEqual(
            bundle.closure(self.hosts)[1].real, self.prefix / "Cellar/brotli/1.2.1/lib/libbrotlidec.1.2.1.dylib",
        )
        self.assertEqual(self.logged(), [])

    def test_an_rpath_name_is_found_through_the_run_paths_of_each_image_back_to_the_host(self):
        dec = self.prefix / "Cellar/brotli/1.2.1/lib/libbrotlidec.1.2.1.dylib"
        macho = load(dec)
        macho["rpaths"] = []
        store(dec, macho)
        with self.assertRaises(Refusal):
            bundle.closure(self.hosts)
        host = load(self.hosts[0])
        host["rpaths"] = ["@executable_path/../../homebrew/opt/brotli/lib"]
        store(self.hosts[0], host)
        store(self.hosts[1], host)
        self.assertIn("libbrotlicommon.1.dylib", [lib.name for lib in bundle.closure(self.hosts)])


class Bundle(Tree):
    def test_the_libraries_are_copied_rewritten_signed_and_listed_with_their_licences(self):
        lines = bundle.bundle(self.stage, self.hosts)
        self.assertEqual(lines, [
            "macos 15.10",
            "lib/libbrotlicommon.1.dylib brotli 1.2.1",
            "lib/libbrotlidec.1.dylib brotli 1.2.1",
            "lib/libcrypto.4.dylib openssl@4 4.1.0",
            "lib/libicudata.79.dylib icu4c@79 79.1",
            "lib/libicuuc.79.dylib icu4c@79 79.1",
            "lib/libjpeg.8.dylib jpeg-turbo 3.2.1",
            "lib/libssl.4.dylib openssl@4 4.1.0",
        ])
        self.assertEqual((self.stage / "bundled-libraries.txt").read_text(), "".join(f"{l}\n" for l in lines))
        lib = self.stage / "lib"
        self.assertEqual(load(self.hosts[0])["names"], [
            "/usr/lib/libSystem.B.dylib", "@loader_path/../lib/libbrotlidec.1.dylib",
            "@loader_path/../lib/libjpeg.8.dylib", "@loader_path/../lib/libicuuc.79.dylib",
            "@loader_path/../lib/libssl.4.dylib", "@loader_path/../lib/libcrypto.4.dylib",
        ])
        self.assertEqual(load(lib / "libbrotlidec.1.dylib"), {
            "id": "@rpath/libbrotlidec.1.dylib",
            "names": ["/usr/lib/libSystem.B.dylib", "@loader_path/libbrotlicommon.1.dylib"],
            "rpaths": [], "minos": "15.0", "signed": True,
        })
        self.assertEqual(load(lib / "libssl.4.dylib")["names"], [
            "@loader_path/libcrypto.4.dylib", "/usr/lib/libSystem.B.dylib",
        ])
        self.assertEqual(load(lib / "libjpeg.8.dylib")["rpaths"], [])
        self.assertEqual(load(lib / "libicuuc.79.dylib")["names"][0], "@loader_path/libicudata.79.dylib")
        self.assertEqual(sorted(p.name for p in lib.iterdir()), [l.split()[0].removeprefix("lib/") for l in lines[1:]])
        for path in lib.iterdir():
            with self.subTest(path=path.name):
                self.assertFalse(path.is_symlink())
                self.assertEqual(path.stat().st_mode & 0o777, 0o644)
                self.assertTrue(load(path)["signed"])
        self.assertTrue(all(load(host)["signed"] for host in self.hosts))
        self.assertEqual(sorted(str(p.relative_to(self.stage)) for p in (self.stage / "licenses").rglob("*")), [
            "licenses/brotli", "licenses/brotli/LICENSE", "licenses/icu4c@79", "licenses/icu4c@79/LICENSE",
            "licenses/jpeg-turbo", "licenses/jpeg-turbo/LICENSE.md",
            "licenses/openssl@4", "licenses/openssl@4/LICENSE.txt",
        ])
        calls = self.logged()
        self.assertIn(["install_name_tool",
                       "-id", "@rpath/libjpeg.8.dylib",
                       "-delete_rpath", f"{self.prefix}/Cellar/jpeg-turbo/3.2.1/lib",
                       str(lib / "libjpeg.8.dylib")], calls)
        self.assertIn(["install_name_tool",
                       "-change", f"{self.prefix}/opt/brotli/lib/libbrotlidec.1.dylib",
                       "@loader_path/../lib/libbrotlidec.1.dylib", str(self.hosts[1])], calls)
        rewritten = [call[-1] for call in calls if call[0] == "install_name_tool"]
        signed = [call[-1] for call in calls if call[0] == "codesign"]
        self.assertEqual(sorted(rewritten), sorted(signed))
        self.assertEqual(len(signed), 2 + 7)
        self.assertEqual(calls.index(next(c for c in calls if c[0] == "codesign")), len(rewritten))
        self.assertEqual(bundle.outside(self.stage), [])

    def test_a_name_found_nowhere_is_refused_saying_which_image_names_it(self):
        dec = self.prefix / "Cellar/brotli/1.2.1/lib/libbrotlidec.1.2.1.dylib"
        macho = load(dec)
        macho["names"].append("@rpath/libbrotlienc.1.dylib")
        store(dec, macho)
        self.refused(f"{self.prefix}/opt/brotli/lib/libbrotlidec.1.dylib loads @rpath/libbrotlienc.1.dylib, "
                     "which is not found from there")

    def test_a_library_in_no_keg_is_refused(self):
        loose = self.prefix / "lib/libloose.1.dylib"
        loose.parent.mkdir()
        store(loose, {"id": str(loose), "names": [], "rpaths": [], "minos": "15.0"})
        host = load(self.hosts[0])
        host["names"].append(str(loose))
        store(self.hosts[0], host)
        self.refused(f"{loose} is in no Homebrew keg, <prefix>/Cellar/<keg>/<version>/")

    def test_a_keg_with_no_licence_is_refused_by_name(self):
        (self.prefix / "Cellar/jpeg-turbo/3.2.1/LICENSE.md").unlink()
        self.refused("the keg jpeg-turbo 3.2.1 carries no licence text, no LICENSE*, LICENCE*, COPYING* or NOTICE*")

    def test_two_libraries_of_one_file_name_are_refused(self):
        other = self.prefix / "Cellar/other/1.0/lib/libjpeg.8.dylib"
        other.parent.mkdir(parents=True)
        store(other, {"id": str(other), "names": [], "rpaths": [], "minos": "15.0"})
        host = load(self.hosts[0])
        host["names"].append(str(other))
        store(self.hosts[0], host)
        self.refused("would both be lib/libjpeg.8.dylib")

    def test_a_name_the_rewrite_leaves_outside_is_refused(self):
        os.environ["IGNORES"] = "1"
        with self.assertRaises(Refusal) as refusal:
            bundle.bundle(self.stage, self.hosts)
        said = str(refusal.exception).splitlines()
        self.assertEqual(said[0], "the stage still names what is outside it and the system's:")
        self.assertIn(f"bin/lela: {self.prefix}/opt/brotli/lib/libbrotlidec.1.dylib", said)
        self.assertIn(f"lib/libbrotlidec.1.dylib: {self.prefix}/opt/brotli/lib/libbrotlidec.1.dylib", said)
        self.assertIn("lib/libbrotlidec.1.dylib: @rpath/libbrotlicommon.1.dylib", said)
        self.assertIn(f"lib/libjpeg.8.dylib: run path {self.prefix}/Cellar/jpeg-turbo/3.2.1/lib", said)
        self.assertFalse((self.stage / "bundled-libraries.txt").exists())


# Three libraries in two kegs of a prefix of the test's own, in the forms
# Homebrew's take: foo's own install name its opt path, a second library of
# foo's by `@loader_path`, bar's by `@rpath` through an absolute run path
# and a relative one; and a host linking foo by its opt path.
FOO = "int bar(void); int help(void); int foo(void) { return bar() + help(); }\n"
BAR = "int bar(void) { return 40; }\n"
HELP = "int help(void) { return 2; }\n"
MAIN = '#include <stdio.h>\nint foo(void);\nint main(void) { printf("%d\\n", foo()); return 0; }\n'


def cc(source, *arguments):
    subprocess.run(["cc", "-x", "c", "-", "-x", "none", *map(str, arguments)], input=source.encode(), check=True, capture_output=True)


def kegs(prefix, host):
    """The prefix's two kegs, and `host` linking foo."""
    foo, bar = prefix / "Cellar/foo/1.0/lib", prefix / "Cellar/bar/2.0_1/lib"
    for lib, licence in ((foo, "LICENSE"), (bar, "COPYING")):
        lib.mkdir(parents=True)
        (lib.parent / licence).write_text("a licence\n")
    (prefix / "opt").mkdir()
    (prefix / "opt/foo").symlink_to("../Cellar/foo/1.0")
    (prefix / "opt/bar").symlink_to("../Cellar/bar/2.0_1")
    cc(BAR, "-dynamiclib", "-o", bar / "libbar.2.0.dylib", "-install_name", "@rpath/libbar.2.dylib")
    (bar / "libbar.2.dylib").symlink_to("libbar.2.0.dylib")
    cc(HELP, "-dynamiclib", "-o", foo / "libfoohelp.1.dylib", "-install_name", "@loader_path/libfoohelp.1.dylib")
    cc(FOO, "-dynamiclib", "-o", foo / "libfoo.1.dylib", "-install_name", prefix / "opt/foo/lib/libfoo.1.dylib",
       bar / "libbar.2.dylib", foo / "libfoohelp.1.dylib", f"-Wl,-rpath,{bar}", "-Wl,-rpath,@loader_path/../lib")
    host.parent.mkdir(parents=True, exist_ok=True)
    cc(MAIN, "-o", host, prefix / "opt/foo/lib/libfoo.1.dylib")


@unittest.skipUnless(os.uname().sysname == "Darwin" and os.uname().machine == "arm64", "an Apple silicon Mac's")
class RealTools(unittest.TestCase):
    def test_a_host_runs_from_the_bundle_with_its_kegs_gone(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary).resolve()
            prefix, stage = scratch / "homebrew", scratch / "stage"
            host = stage / "bin/lela"
            kegs(prefix, host)
            smoke = [shutil.which("bash"), str(DIST / "smoke.sh"), "--machos", str(stage)]
            before = subprocess.run(smoke, capture_output=True, text=True)
            self.assertEqual(before.returncode, 1, before.stdout)
            self.assertIn(f"bin/lela: {prefix}/opt/foo/lib/libfoo.1.dylib", before.stderr)
            lines = bundle.bundle(stage, [host])
            self.assertEqual(lines[1:], [
                "lib/libbar.2.dylib bar 2.0_1", "lib/libfoo.1.dylib foo 1.0", "lib/libfoohelp.1.dylib foo 1.0",
            ])
            self.assertRegex(lines[0], r"^macos \d+\.\d+")
            self.assertEqual(bundle.outside(stage), [])
            after = subprocess.run(smoke, capture_output=True, text=True)
            self.assertEqual(after.returncode, 0, after.stderr)
            self.assertEqual(after.stdout, "Mach-O: 4 files name only the system's libraries and the tarball's own, "
                                           "each signed\n")
            for path in bundle.machos(stage):
                with self.subTest(path=path.name):
                    subprocess.run(["codesign", "--verify", "--strict", str(path)], check=True)
            # The kegs gone and the tree moved: what the host loads is beside it.
            shutil.rmtree(prefix)
            moved = scratch / "moved"
            stage.rename(moved)
            run = subprocess.run([str(moved / "bin/lela")], capture_output=True, text=True)
            self.assertEqual((run.returncode, run.stdout), (0, "42\n"), run.stderr)


if __name__ == "__main__":
    unittest.main()
