"""The installer, dist/install.sh, as dist/release.py stages it for a
release, run under sh (and dash, where the machine has it) over a release
of the test's own, with no network.

Each test lays a release in a directory, its tarball packed by the
release's own `pack` and its sums written by scripts/dist-sums.sh, and
points LELA_INSTALL_FROM at it as a file:// URL; HOME is a scratch home.
The tarball's hosts are scripts that print a banner. The script runs on a
PATH of the test's own: stand-ins for what says which machine this is
(`uname`, `sysctl`, `sw_vers`, `dpkg-query`, and `id` where a test runs
as root), and links to the few programs it calls that are no such thing.
The os-release file it reads is the test's, through LELA_OS_RELEASE.
"""

import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import publish  # noqa: E402
import release  # noqa: E402

ROOT = release.ROOT
# The programs the installer calls that are no stand-in, linked where the
# machine has them.
TOOLS = (
    "awk", "cat", "curl", "dirname", "grep", "gzip", "id", "ln", "mkdir", "mktemp", "mv", "readlink", "rm",
    "rmdir", "sed", "sha256sum", "shasum", "tar",
)
UBUNTU = 'NAME="Ubuntu"\nVERSION_ID="26.04"\nID=ubuntu\nID_LIKE=debian\nPRETTY_NAME="Ubuntu 26.04 LTS"\n'
DEBIAN = 'PRETTY_NAME="Debian GNU/Linux 13 (trixie)"\nNAME="Debian GNU/Linux"\nVERSION_ID="13"\nID=debian\n'
NOBLE = 'PRETTY_NAME="Ubuntu 24.04.3 LTS"\nNAME="Ubuntu"\nVERSION_ID="24.04"\nID=ubuntu\n'
PACKAGES = ("libc6", "libssl3t64", "libicu76")


def banner(version):
    """The first line of `lela -v` as a release of the version prints it:
    php's, with Lela's version after it."""
    return f"PHP 8.5.12 (cli) (built: Nov 21 2026 12:00:00) (NTS) [Lela {version}]"


def shells():
    """sh, and dash where the machine has it beside."""
    found = [shutil.which("sh")]
    dash = shutil.which("dash")
    if dash and os.path.realpath(dash) != os.path.realpath(found[0]):
        found.append(dash)
    return found


class Installer:
    """The cases, run under `shell` by each class below."""

    shell = None
    version = "1.2.0"

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name).resolve()
        self.home = self.scratch / "home"
        self.home.mkdir()
        self.releases = self.scratch / "releases"
        self.stand = self.scratch / "bin"
        self.stand.mkdir()
        self.tools = self.scratch / "tools"
        self.tools.mkdir()
        for tool in TOOLS:
            found = shutil.which(tool)
            if found:
                (self.tools / tool).symlink_to(found)
        self.os_release = self.scratch / "os-release"
        self.machine("Darwin", "arm64")
        self.program("sw_vers", "echo 15.6")
        self.installed = self.scratch / "installed"
        self.installed.write_text("".join(f"{package}\n" for package in PACKAGES))
        # A dpkg-query that says a package of the file `installed` is.
        self.program("dpkg-query", f'''
for package; do :; done
if grep -qx "$package" "{self.installed}"; then echo "install ok installed"
else echo "dpkg-query: no packages found matching $package" >&2; exit 1; fi''')
        self.os_release.write_text(UBUNTU)

    # --- the machine --------------------------------------------------------

    def program(self, name, body):
        path = self.stand / name
        path.write_text(f"#!/bin/sh\n{body}\n")
        path.chmod(0o755)

    def machine(self, system, machine, arm64=None):
        """`uname -s` and `uname -m`, and the `sysctl -n hw.optional.arm64`
        an Intel Mac or Rosetta answers."""
        self.program("uname", f'case "$1" in -s) echo {system} ;; -m) echo {machine} ;; *) exit 1 ;; esac')
        if arm64 is None:
            (self.stand / "sysctl").unlink(missing_ok=True)
        else:
            self.program("sysctl", f"echo {arm64}")

    def linux(self, machine="x86_64", os_release=UBUNTU):
        self.machine("Linux", machine)
        self.os_release.write_text(os_release)

    # --- the release --------------------------------------------------------

    def tarball(self, platform, version=None, says=None, fails=False, floor="14.0", body=None):
        """One platform's tarball in the release's directory, laid as the
        staging lays one, and the release's SHA256SUMS written again. Its
        hosts print `says` as the first line of their banner, php's with
        the release's version after it where it is not given, then the
        platform, or run `body` in place of printing."""
        version = version or self.version
        base = f"lela-{version}-{platform}"
        out = self.releases / version
        stage = self.scratch / "stages" / base
        shutil.rmtree(stage, ignore_errors=True)
        says = says or banner(version)
        body = body or f"printf '%s\\n' '{says}' 'the {platform} build'"
        for host in (host.path for host in publish.in_tarball()):
            path = stage / host
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"#!/bin/sh\n{body}\n" + ("exit 3\n" if fails else ""))
            path.chmod(0o755)
        (stage / "etc").mkdir()
        for ini in ("php.ini-development", "php.ini-production"):
            (stage / "etc" / ini).write_text(ini)
        if platform.startswith("darwin"):
            (stage / "lib").mkdir()
            (stage / "lib/libzz.1.dylib").write_text("zz")
            (stage / "bundled-libraries.txt").write_text(f"macos {floor}\nlib/libzz.1.dylib zz 1.0_1\n")
        else:
            (stage / "runtime-packages.txt").write_text("".join(f"{package}\n" for package in PACKAGES))
        out.mkdir(parents=True, exist_ok=True)
        release.pack(stage, out / f"{base}.tar.gz")
        subprocess.run(
            ["bash", str(ROOT / "scripts/dist-sums.sh"), str(out), *sorted(p.name for p in out.glob("*.tar.gz"))],
            check=True, capture_output=True,
        )
        return out

    def script(self, version=None):
        """dist/install.sh as dist/release.py stages it for the version."""
        path = self.scratch / f"install-{version or self.version}.sh"
        path.write_text(release.installer(publish.Ref(version or self.version)))
        return path

    def install(self, version=None, script=None, path=None, stdin=False, **env):
        version = version or self.version
        script = script or self.script(version)
        environment = {
            "HOME": str(self.home),
            "PATH": path or os.pathsep.join([str(self.stand), str(self.tools)]),
            "LELA_INSTALL_FROM": (self.releases / version).as_uri(),
            "LELA_OS_RELEASE": str(self.os_release),
            **env,
        }
        if stdin:
            return subprocess.run(
                [self.shell], input=script.read_text(), env=environment, cwd=self.scratch,
                capture_output=True, text=True,
            )
        return subprocess.run(
            [self.shell, str(script)], env=environment, cwd=self.scratch, capture_output=True, text=True,
        )

    # --- what a run leaves --------------------------------------------------

    def tree(self):
        """Every path under the home, a link by its target."""
        listed = {}
        for path in sorted(self.home.rglob("*")):
            relative = str(path.relative_to(self.home))
            listed[relative] = f"-> {os.readlink(path)}" if path.is_symlink() else "dir" if path.is_dir() else "file"
        return listed

    def link(self):
        return self.home / ".local/bin/lela"

    def data(self, version=None):
        return self.home / ".local/share/lela" / (version or self.version)

    def assertInstalled(self, run, platform, version=None):
        version = version or self.version
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(os.readlink(self.link()), str(self.data(version) / "bin/lela"))
        printed = run.stdout.splitlines()
        self.assertIn(banner(version), printed)
        self.assertEqual(printed[printed.index(banner(version)) + 1], f"the {platform} build")
        self.assertIn(
            f"install.sh: installed lela {version} in {self.data(version)}, "
            f"with {self.link()} a link to its bin/lela",
            run.stdout,
        )
        for path in ("bin/lela-cgi", "sbin/lela-fpm", "bin/lela-dbg", "etc/php.ini-production"):
            self.assertTrue((self.data(version) / path).is_file(), path)
        # No stage is left beside the installed release.
        self.assertEqual(sorted(p.name for p in self.data(version).parent.iterdir()), [version])

    def assertRefused(self, run, said, before=None):
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"install.sh: {said}", run.stderr)
        self.assertEqual(self.tree(), before or {})

    # --- the cases ----------------------------------------------------------

    def test_a_mac_on_apple_silicon_gets_the_darwin_tarball_linked_into_local_bin(self):
        self.tarball("darwin-arm64")
        run = self.install()
        self.assertInstalled(run, "darwin-arm64")
        self.assertTrue((self.data() / "lib/libzz.1.dylib").is_file())
        self.assertIn(f"install.sh: {self.home}/.local/bin is not on PATH", run.stdout)
        self.assertIn('export PATH="$HOME/.local/bin:$PATH"', run.stdout)

    def test_piped_into_sh_it_installs_the_same(self):
        self.tarball("darwin-arm64")
        self.assertInstalled(self.install(stdin=True), "darwin-arm64")

    def test_ubuntu_on_either_architecture_gets_its_linux_tarball(self):
        for machine, platform in (("x86_64", "linux-x86_64"), ("aarch64", "linux-aarch64")):
            with self.subTest(platform=platform):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                self.linux(machine)
                self.tarball(platform)
                self.assertInstalled(self.install(), platform)

    def test_a_shell_under_rosetta_on_apple_silicon_gets_the_darwin_tarball(self):
        self.machine("Darwin", "x86_64", arm64=1)
        self.tarball("darwin-arm64")
        self.assertInstalled(self.install(), "darwin-arm64")

    def test_xdg_data_home_holds_the_release_where_it_is_set(self):
        self.tarball("darwin-arm64")
        data = self.home / "data"
        run = self.install(XDG_DATA_HOME=str(data))
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(os.readlink(self.link()), str(data / "lela/1.2.0/bin/lela"))
        self.assertFalse((self.home / ".local/share").exists())

    def test_with_local_bin_on_path_nothing_is_said_of_path(self):
        self.tarball("darwin-arm64")
        bin = self.home / ".local/bin"
        run = self.install(path=os.pathsep.join([str(self.stand), str(self.tools), str(bin)]))
        self.assertInstalled(run, "darwin-arm64")
        self.assertNotIn("PATH", run.stdout)

    def test_a_php_and_every_other_file_of_the_home_is_left_as_it_is(self):
        self.tarball("darwin-arm64")
        bin = self.home / ".local/bin"
        bin.mkdir(parents=True)
        (bin / "php").write_text("#!/bin/sh\necho the user's php\n")
        (bin / "php").chmod(0o755)
        (self.home / ".profile").write_text("# the user's\n")
        self.assertInstalled(self.install(), "darwin-arm64")
        self.assertEqual((bin / "php").read_text(), "#!/bin/sh\necho the user's php\n")
        self.assertEqual((self.home / ".profile").read_text(), "# the user's\n")

    def test_a_tarball_whose_checksum_differs_is_refused_and_nothing_is_installed(self):
        out = self.tarball("darwin-arm64")
        tarball = out / "lela-1.2.0-darwin-arm64.tar.gz"
        wanted = hashlib.sha256(tarball.read_bytes()).hexdigest()
        with tarball.open("ab") as file:
            file.write(b"\0")
        got = hashlib.sha256(tarball.read_bytes()).hexdigest()
        run = self.install()
        self.assertRefused(
            run, f"lela-1.2.0-darwin-arm64.tar.gz's SHA-256 is {got}, and the release's SHA256SUMS says {wanted}, "
            "so it is not installed",
        )

    def test_a_tarball_the_sums_do_not_name_is_refused_and_nothing_is_installed(self):
        out = self.tarball("darwin-arm64")
        sums = out / "SHA256SUMS"
        sums.write_text("".join(line + "\n" for line in sums.read_text().splitlines() if "darwin" not in line))
        self.assertRefused(
            self.install(), "the release's SHA256SUMS names no lela-1.2.0-darwin-arm64.tar.gz, so it is not installed",
        )

    def test_a_release_with_no_sums_or_no_tarball_is_refused(self):
        out = self.tarball("darwin-arm64")
        (out / "lela-1.2.0-darwin-arm64.tar.gz").unlink()
        self.assertRefused(self.install(), f"could not download {out.as_uri()}/lela-1.2.0-darwin-arm64.tar.gz")
        (out / "SHA256SUMS").unlink()
        self.assertRefused(self.install(), f"could not download {out.as_uri()}/SHA256SUMS")

    def test_a_linux_that_is_not_ubuntu_is_refused_by_name(self):
        self.linux(os_release=DEBIAN)
        self.tarball("linux-x86_64")
        self.assertRefused(
            self.install(),
            f"this is Debian GNU/Linux 13 (trixie) ({self.os_release}), "
            "and lela's Linux tarballs are built for Ubuntu 26.04 alone",
        )

    def test_an_ubuntu_of_another_release_is_refused_by_name(self):
        self.linux(os_release=NOBLE)
        self.tarball("linux-x86_64")
        self.assertRefused(
            self.install(),
            f"this is Ubuntu 24.04.3 LTS ({self.os_release}), and lela's Linux tarballs are built for Ubuntu 26.04 alone",
        )

    def test_a_linux_with_no_os_release_is_refused(self):
        self.linux()
        self.tarball("linux-x86_64")
        self.os_release.unlink()
        self.assertRefused(self.install(), "no /etc/os-release says which Linux this is")

    def test_an_unsupported_platform_is_refused_and_nothing_is_fetched(self):
        for system, machine in (("FreeBSD", "amd64"), ("Linux", "riscv64"), ("Linux", "armv7l"), ("MINGW64_NT-10.0", "x86_64")):
            with self.subTest(system=system, machine=machine):
                self.machine(system, machine)
                run = self.install()
                self.assertRefused(
                    run, f"lela 1.2.0 has no tarball for {system} {machine}; "
                    "its platforms are darwin-arm64, linux-aarch64 and linux-x86_64",
                )
                self.assertNotIn("downloading", run.stdout)

    def test_a_platform_added_to_the_table_is_installed_or_refused_as_its_row_says(self):
        added = (
            release.Platform(
                "linux-riscv64", docker="linux/riscv64", uname=("Linux riscv64",), checks="ubuntu", installs=True,
            ),
            release.Platform("linux-s390x", docker="linux/s390x", uname=("Linux s390x",), checks=None, installs=False),
        )
        with mock.patch.object(release, "PLATFORMS", (*release.PLATFORMS, *added)):
            script = self.script()
        self.linux("riscv64")
        self.tarball("linux-riscv64")
        self.assertInstalled(self.install(script=script), "linux-riscv64")
        shutil.rmtree(self.home / ".local")
        self.linux("s390x")
        self.tarball("linux-s390x")
        self.assertRefused(
            self.install(script=script), "lela 1.2.0's tarball for linux-s390x is not one this installer installs; "
            "download it from https://github.com/lelaphp/lela/releases/download/v1.2.0",
        )
        self.machine("FreeBSD", "amd64")
        self.assertRefused(
            self.install(script=script), "lela 1.2.0 has no tarball for FreeBSD amd64; "
            "its platforms are darwin-arm64, linux-aarch64, linux-x86_64 and linux-riscv64",
        )

    def test_the_checks_a_machine_is_held_to_are_its_rows_not_its_systems(self):
        # A row of a system that is not Linux, held to the Ubuntu checks: its
        # os-release and packages are read as an Ubuntu's are.
        added = release.Platform("bsd-x86_64", docker=None, uname=("FreeBSD amd64",), checks="ubuntu", installs=True)
        with mock.patch.object(release, "PLATFORMS", (*release.PLATFORMS, added)):
            script = self.script()
        self.machine("FreeBSD", "amd64")
        self.os_release.write_text(DEBIAN)
        self.tarball("bsd-x86_64")
        self.assertRefused(
            self.install(script=script),
            f"this is Debian GNU/Linux 13 (trixie) ({self.os_release}), "
            "and lela's Linux tarballs are built for Ubuntu 26.04 alone",
        )
        self.os_release.write_text(UBUNTU)
        self.assertInstalled(self.install(script=script), "bsd-x86_64")
        # And a Linux row held to the macOS checks is held to the tarball's
        # floor, not to an os-release.
        mac = release.Platform("linux-mac", docker=None, uname=("Linux riscv64",), checks="macos", installs=True)
        with mock.patch.object(release, "PLATFORMS", (*release.PLATFORMS, mac)):
            script = self.script()
        shutil.rmtree(self.home / ".local")
        self.linux("riscv64")
        self.os_release.unlink()
        self.tarball("linux-mac")
        self.assertRefused(self.install(script=script), "lela-1.2.0-linux-mac.tar.gz's bundled-libraries.txt names "
                           "no oldest macOS on its first line")

    def test_an_intel_mac_is_refused(self):
        for arm64 in (0, None):
            with self.subTest(sysctl=arm64):
                self.machine("Darwin", "x86_64", arm64=arm64)
                self.assertRefused(
                    self.install(),
                    "this is an Intel Mac, and lela 1.2.0's macOS tarball is built for Apple silicon alone",
                )

    def test_a_macos_older_than_the_tarballs_floor_is_refused_by_name(self):
        self.tarball("darwin-arm64", floor="14.0")
        for here, named in (("13.6.1", "macOS 13.6.1 Ventura"), ("10.15.7", "macOS 10.15.7 Catalina")):
            with self.subTest(macos=here):
                self.program("sw_vers", f"echo {here}")
                self.assertRefused(
                    self.install(), f"this Mac runs {named}, and lela 1.2.0 runs on macOS 14.0 Sonoma or later",
                )
        for here in ("14.0", "14.7.2", "26.1"):
            with self.subTest(macos=here):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                self.program("sw_vers", f"echo {here}")
                self.assertInstalled(self.install(), "darwin-arm64")

    def test_the_packages_ubuntu_lacks_are_named_with_the_line_that_installs_them(self):
        self.linux()
        self.tarball("linux-x86_64")
        self.installed.write_text("libc6\n")
        run = self.install()
        self.assertRefused(run, "lela 1.2.0 is not installed")
        self.assertIn(
            "install.sh: lela 1.2.0 loads libraries from these Ubuntu packages, which are not installed: "
            "libssl3t64 libicu76", run.stderr,
        )
        self.assertIn("\n    sudo apt-get install libssl3t64 libicu76\n", run.stderr)

    def test_a_rerun_replaces_the_release_in_place_and_an_upgrade_removes_the_old_one(self):
        self.tarball("darwin-arm64")
        self.assertInstalled(self.install(), "darwin-arm64")
        (self.data() / "etc/php.ini-production").write_text("changed by hand")
        self.assertInstalled(self.install(), "darwin-arm64")
        self.assertEqual((self.data() / "etc/php.ini-production").read_text(), "php.ini-production")
        self.tarball("darwin-arm64", version="1.3.0")
        self.assertInstalled(self.install(version="1.3.0"), "darwin-arm64", version="1.3.0")
        self.assertFalse(self.data("1.2.0").exists())

    def test_a_release_whose_lela_does_not_run_leaves_the_one_installed(self):
        self.tarball("darwin-arm64")
        self.assertInstalled(self.install(), "darwin-arm64")
        before = self.tree()
        self.tarball("darwin-arm64", version="1.3.0", fails=True)
        run = self.install(version="1.3.0")
        self.assertRefused(run, "the unpacked lela -v fails, so it is not installed", before)

    def test_a_release_whose_lela_names_another_release_is_refused_and_leaves_the_one_installed(self):
        self.tarball("darwin-arm64")
        self.assertInstalled(self.install(), "darwin-arm64")
        before = self.tree()
        for says in (banner("1.2.0"), banner("1.3.0-rc.1"), banner("1.3.0") + " ",
                     "PHP 8.5.12 (cli) (built: Nov 21 2026 12:00:00) (NTS)",
                     "PHP 8.5.12 (cli) (built: Nov 21 2026 12:00:00) (NTS) [Lela 1.3.0] [Lela 1.2.0]",
                     "PHP 8.5.12 (cli) (built: Nov 21 2026 12:00:00) (NTS) [lela 1.3.0]"):
            with self.subTest(says=says):
                self.tarball("darwin-arm64", version="1.3.0", says=says)
                self.assertRefused(
                    self.install(version="1.3.0"),
                    f"the unpacked lela -v says {says}, where lela 1.3.0's first line ends [Lela 1.3.0], "
                    "so it is not installed", before,
                )
        # The version on a later line alone is not the first line's.
        self.tarball("darwin-arm64", version="1.3.0",
                     body=f"printf '%s\\n' '{banner('1.2.0')}' '{banner('1.3.0')}'")
        self.assertRefused(
            self.install(version="1.3.0"),
            f"the unpacked lela -v says {banner('1.2.0')}, where lela 1.3.0's first line ends [Lela 1.3.0]", before,
        )

    def test_a_lela_that_names_another_release_through_its_link_is_undone(self):
        # Its own release's while it is unpacked in the stage, another's
        # once it runs from where it is put.
        body = (f"case \"$0\" in */.install.*) echo '{banner('1.3.0')}' ;; "
                f"*) echo '{banner('1.2.0')}' ;; esac")
        said = (f"{self.link()} -v says {banner('1.2.0')}, where lela 1.3.0's first line ends [Lela 1.3.0], "
                "so lela 1.3.0 is not installed")
        self.tarball("darwin-arm64", version="1.3.0", body=body)
        self.assertRefused(self.install(version="1.3.0"), said)
        self.tarball("darwin-arm64")
        self.assertInstalled(self.install(), "darwin-arm64")
        before = self.tree()
        self.assertRefused(self.install(version="1.3.0"), said, before)

    def test_a_lela_in_local_bin_that_is_not_the_installers_is_left_and_refused(self):
        self.tarball("darwin-arm64")
        bin = self.home / ".local/bin"
        bin.mkdir(parents=True)
        (bin / "lela").write_text("mine")
        before = self.tree()
        self.assertRefused(
            self.install(), f"{bin}/lela is already there, and is not this installer's link; "
            "move it away, then run this again", before,
        )
        (bin / "lela").unlink()
        (bin / "lela").symlink_to(self.scratch / "lela/target/release/lela")
        before = self.tree()
        self.assertRefused(self.install(), f"{bin}/lela is a link of another's", before)

    def test_with_neither_curl_nor_wget_it_says_so_and_fetches_nothing(self):
        self.tarball("darwin-arm64")
        (self.tools / "curl").unlink()
        self.assertRefused(
            self.install(), "neither curl nor wget is on PATH, so the release cannot be downloaded; "
            "install one, then run this again",
        )

    def test_wget_fetches_where_curl_is_not(self):
        self.tarball("darwin-arm64")
        (self.tools / "curl").unlink()
        # A wget of file:// URLs alone, which the real one has no scheme for.
        self.program("wget", '''
[ "$1 $2" = "-q -O" ] || exit 2
cp "${4#file://}" "$3"''')
        (self.tools / "cp").symlink_to(shutil.which("cp"))
        self.assertInstalled(self.install(), "darwin-arm64")

    def test_under_sudo_it_is_refused(self):
        self.tarball("darwin-arm64")
        (self.tools / "id").unlink()
        self.program("id", "echo 0")
        self.assertRefused(self.install(SUDO_USER="someone"), "run this without sudo")

    def test_the_source_names_no_release_and_is_refused(self):
        self.tarball("darwin-arm64")
        self.assertRefused(
            self.install(script=ROOT / "dist/install.sh"),
            "this is the installer's source, which names no release; run the install.sh attached to one",
        )


@unittest.skipUnless(shutil.which("shellcheck"), "shellcheck is not on PATH")
class Shellcheck(unittest.TestCase):
    def test_the_installer_is_posix_sh_to_shellcheck(self):
        run = subprocess.run(
            ["shellcheck", "-s", "sh", str(ROOT / "dist/install.sh")], capture_output=True, text=True,
        )
        self.assertEqual(run.returncode, 0, run.stdout)


SHELLS = shells()


class UnderSh(Installer, unittest.TestCase):
    shell = SHELLS[0]


@unittest.skipUnless(len(SHELLS) > 1, "no dash beside sh on this machine")
class UnderDash(Installer, unittest.TestCase):
    shell = SHELLS[-1]


if __name__ == "__main__":
    unittest.main()
