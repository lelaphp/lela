"""The smoke's arguments, held without Docker or a build."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import publish

ROOT = Path(__file__).resolve().parent.parent
# The programs the checks run beyond the hosts and the shell's own, which a
# test's PATH links from the machine, and no directory of the system's,
# where a runner may keep a php.
TOOLS = ("mktemp", "rm", "sort", "xargs", "id")


def tools(directory):
    """`directory`, made, with a link to each of TOOLS."""
    directory.mkdir()
    for tool in TOOLS:
        (directory / tool).symlink_to(shutil.which(tool))
    return directory


class SmokeArguments(unittest.TestCase):
    def smoke(self, *arguments):
        # Nothing on PATH, docker least of all: a refusal comes before any
        # command the script runs.
        return subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), *arguments],
            env={**os.environ, "PATH": ""},
            capture_output=True, text=True,
        )

    def test_a_variant_the_images_do_not_have_is_refused_before_docker_runs(self):
        for arguments in (("--image", "lela:nightly", "zts"), ("--image", "lela:nightly"), ("--image",)):
            with self.subTest(arguments=arguments):
                run = self.smoke(*arguments)
                self.assertEqual(run.returncode, 2, run.stderr)
                self.assertIn("--image <image> cli|fpm|apache", run.stderr)

    def test_no_directory_or_two_are_refused(self):
        for arguments in ((), ("a", "b"), ("--unknown",), ("--machos",), ("--machos", "a", "b"),
                          ("--https",), ("--https", "a", "b"), ("--openssl",), ("--openssl", "a", "b")):
            with self.subTest(arguments=arguments):
                run = self.smoke(*arguments)
                self.assertEqual(run.returncode, 2, run.stderr)
                self.assertIn("usage:", run.stderr)


class Https(unittest.TestCase):
    """The https requests, `--https <dir>`, run over a directory whose
    `bin/lela` answers each URL as the test says, beside a `curl` that
    reaches each host or none, with no brew on PATH, so no sandbox."""

    TRUSTED = "https://www.php.net/"
    EXPIRED = "https://expired.badssl.com/"

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name).resolve()
        (self.scratch / "tarball/bin").mkdir(parents=True)
        (self.scratch / "path").mkdir()

    def requests(self, curl_exit, answers):
        """The requests, curl exiting `curl_exit` and the host reading each
        URL of `answers` and refusing any other; and the host's environment
        and arguments, one line per request."""
        seen = self.scratch / "seen"
        cases = "".join(f'  *"{url}"*) echo {answer} ;;\n' for url, answer in answers.items())
        host = self.scratch / "tarball/bin/lela"
        host.write_text(
            f'#!/bin/sh\necho "SSL_CERT_FILE=${{SSL_CERT_FILE-unset}} $*" >> {seen}\n'
            f'case "$*" in\n{cases}  *) echo refused ;;\nesac\n'
        )
        curl = self.scratch / "path/curl"
        curl.write_text(f"#!/bin/sh\nexit {curl_exit}\n")
        for program in (host, curl):
            program.chmod(0o755)
        run = subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), "--https", str(self.scratch / "tarball")],
            env={**os.environ, "NAME": "lela", "SSL_CERT_FILE": "/elsewhere.pem",
                 "PATH": os.pathsep.join([str(self.scratch / "path"), "/usr/bin", "/bin"])},
            capture_output=True, text=True,
        )
        return run, seen.read_text().splitlines() if seen.exists() else []

    def test_the_trusted_host_is_read_and_the_expired_one_refused(self):
        run, seen = self.requests(0, {self.TRUSTED: "read"})
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("https, a trusted certificate: read", run.stdout)
        self.assertIn("https, an expired certificate: refused", run.stdout)
        self.assertEqual(len(seen), 2)
        for line in seen:
            with self.subTest(line=line):
                self.assertTrue(line.startswith("SSL_CERT_FILE=unset -n "), line)

    def test_a_stream_that_reads_the_expired_host_fails(self):
        run, _ = self.requests(0, {self.TRUSTED: "read", self.EXPIRED: "read"})
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("https, an expired certificate: wanted refused", run.stderr)

    def test_a_stream_that_refuses_the_trusted_host_fails(self):
        run, _ = self.requests(0, {})
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("https, a trusted certificate: wanted read", run.stderr)

    def test_with_no_network_no_stream_is_tried_and_the_line_says_so(self):
        run, seen = self.requests(7, {})
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertEqual(seen, [])
        for url in (self.TRUSTED, self.EXPIRED):
            with self.subTest(url=url):
                self.assertIn(f"curl reaches nothing at {url} from this machine, so the stream is not tried",
                              run.stdout)


class OpensslConf(unittest.TestCase):
    """The openssl.cnf read, `--openssl <dir>`, over a directory whose
    `bin/lela` names the library's compiled-in directory, the file it reads
    and whether a key and a request are made, as the test says, with no brew
    on PATH, so no sandbox."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name).resolve()
        (self.scratch / "tarball/bin").mkdir(parents=True)
        (self.scratch / "tarball/lib").mkdir()
        (self.scratch / "tarball/lib/openssl.cnf").write_text("[ req ]\n")
        self.laid = self.scratch / "tarball/lib/openssl.cnf"
        self.area = self.scratch / "homebrew/etc/openssl@4"

    def conf(self, reads, made="made", compiled_in=True):
        """The smoke over the host, reading `reads` and answering `made`
        for the key and the request, with the compiled-in file there or
        not; and the host's environment, one line per run."""
        if compiled_in:
            self.area.mkdir(parents=True)
            (self.area / "openssl.cnf").write_text("[ req ]\n")
        seen = self.scratch / "seen"
        host = self.scratch / "tarball/bin/lela"
        host.write_text(
            f'#!/bin/sh\necho "OPENSSL_CONF=${{OPENSSL_CONF-unset}} SSLEAY_CONF=${{SSLEAY_CONF-unset}}" >> {seen}\n'
            f'case "$*" in\n'
            f'  *openssl_get_cert_locations*) echo {self.area}/openssl.cnf ;;\n'
            f'  *ReflectionExtension*) echo {reads} ;;\n'
            f'  *openssl_pkey_new*) echo {made} ;;\n'
            f'esac\n'
        )
        host.chmod(0o755)
        run = subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), "--openssl", str(self.scratch / "tarball")],
            env={**os.environ, "NAME": "lela", "OPENSSL_CONF": "/elsewhere.cnf", "SSLEAY_CONF": "/elsewhere.cnf",
                 "PATH": os.pathsep.join(["/usr/bin", "/bin"])},
            capture_output=True, text=True,
        )
        return run, seen.read_text().splitlines() if seen.exists() else []

    def test_the_compiled_in_file_is_read_where_it_is_there(self):
        run, seen = self.conf(self.area / "openssl.cnf")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn(f"openssl.cnf: {self.area}/openssl.cnf", run.stdout)
        self.assertIn("openssl, a key and a request with no config option: made", run.stdout)
        self.assertEqual(seen, ["OPENSSL_CONF=unset SSLEAY_CONF=unset"] * 3)

    def test_the_tarballs_file_is_read_where_the_compiled_in_one_is_not(self):
        run, _ = self.conf(self.laid, compiled_in=False)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn(f"openssl.cnf: {self.laid}", run.stdout)

    def test_a_host_reading_the_tarballs_file_beside_the_compiled_in_one_fails(self):
        run, _ = self.conf(self.laid)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"openssl.cnf: wanted {self.area}/openssl.cnf", run.stderr)

    def test_a_host_reading_the_absent_compiled_in_file_fails(self):
        run, _ = self.conf(self.area / "openssl.cnf", compiled_in=False)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"openssl.cnf: wanted {self.laid}", run.stderr)

    def test_a_key_or_a_request_refused_fails(self):
        run, _ = self.conf(self.area / "openssl.cnf", made="refused")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("openssl, a key and a request with no config option: wanted made", run.stderr)


class Shipped(unittest.TestCase):
    """The hosts the smoke holds a tarball and each image to, `--shipped
    tarball|<variant>`: dist/publish.py's HOSTS, which a test holds to
    `cargo xtask hosts --list`, in its order."""

    def shipped(self, subject):
        run = subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), "--shipped", subject],
            capture_output=True, text=True,
        )
        self.assertEqual(run.returncode, 0, run.stderr)
        return run.stdout.split()

    def test_the_tarball_and_each_image_hold_the_hosts_the_table_ships_there(self):
        self.assertEqual(self.shipped("tarball"), [host.path for host in publish.in_tarball()])
        for variant in publish.VARIANTS:
            with self.subTest(variant=variant):
                self.assertEqual(self.shipped(variant), [host.path for host in publish.in_image(variant)])
        self.assertEqual(self.shipped("cli"), ["bin/lela", "bin/lela-cgi", "bin/lela-dbg"])


# The hosts as the checks call them, each answering what the checks want
# and nothing else: the debugger a file only through `-qrr`.
HOSTS = {
    "lela": 'case "$*" in -v) echo lela ;; "-n -r echo 1 + 1;") echo 2 ;; *args.php*) echo "a,b c" ;; '
            '-n) echo OK ;; *) exit 1 ;; esac',
    "lela-cgi": 'case "$*" in "-n -q "*) echo cgi-fcgi ;; *) exit 1 ;; esac',
    "lela-fpm": 'case "$*" in "-n -t -y "*) ;; *) exit 1 ;; esac',
    "lela-dbg": 'case "$*" in "-n -qrr "*) echo phpdbg ;; *) exit 1 ;; esac',
}


class Hosts(unittest.TestCase):
    """The hosts check of either mode, `--hosts <prefix> tarball|<variant>`,
    over a prefix of the test's own whose hosts answer as HOSTS says."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name).resolve()
        self.prefix = self.scratch / "prefix"
        self.tools = tools(self.scratch / "tools")

    def lay(self, *paths):
        for path in paths:
            host = self.prefix / path
            host.parent.mkdir(parents=True, exist_ok=True)
            host.write_text(f"#!/bin/sh\n{HOSTS[host.name]}\n")
            host.chmod(0o755)

    def check(self, subject):
        return subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), "--hosts", str(self.prefix), subject],
            env={**os.environ, "NAME": "lela", "PATH": str(self.tools)},
            capture_output=True, text=True,
        )

    def test_each_host_the_subject_holds_is_checked_the_debugger_among_them(self):
        for subject, paths in (
            ("tarball", ("bin/lela", "bin/lela-cgi", "sbin/lela-fpm", "bin/lela-dbg")),
            ("cli", ("bin/lela", "bin/lela-cgi", "bin/lela-dbg")),
            ("fpm", ("bin/lela", "bin/lela-cgi", "sbin/lela-fpm")),
        ):
            with self.subTest(subject=subject):
                shutil.rmtree(self.prefix, ignore_errors=True)
                self.lay(*paths)
                run = self.check(subject)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertIn(f"hosts: {' '.join(paths)}, and no other", run.stdout)
                self.assertIn("lela-cgi: cgi-fcgi", run.stdout)
                self.assertEqual("lela-dbg: phpdbg" in run.stdout, "bin/lela-dbg" in paths, run.stdout)
                self.assertEqual("lela-fpm -t: exit 0" in run.stdout, "sbin/lela-fpm" in paths, run.stdout)

    def test_an_image_that_holds_a_host_it_should_not_fails(self):
        self.lay("bin/lela", "bin/lela-cgi", "sbin/lela-fpm", "bin/lela-dbg")
        for variant in ("fpm", "apache"):
            with self.subTest(variant=variant):
                run = self.check(variant)
                self.assertEqual(run.returncode, 1, run.stdout)
                self.assertIn(
                    f"hosts: {self.prefix} holds bin/lela bin/lela-cgi bin/lela-dbg sbin/lela-fpm, "
                    f"where the {variant} holds bin/lela bin/lela-cgi sbin/lela-fpm",
                    run.stderr,
                )

    def test_a_subject_that_lacks_a_host_fails(self):
        self.lay("bin/lela", "bin/lela-cgi", "sbin/lela-fpm")
        for subject in ("cli", "tarball"):
            with self.subTest(subject=subject):
                run = self.check(subject)
                self.assertEqual(run.returncode, 1, run.stdout)
                self.assertIn(f"hosts: {self.prefix} holds bin/lela bin/lela-cgi sbin/lela-fpm, where the {subject}",
                              run.stderr)

    def test_a_debugger_that_does_not_run_its_file_fails(self):
        self.lay("bin/lela", "bin/lela-cgi")
        (self.prefix / "bin/lela-dbg").write_text("#!/bin/sh\necho prompt\n")
        (self.prefix / "bin/lela-dbg").chmod(0o755)
        run = self.check("cli")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("lela-dbg: wanted phpdbg", run.stderr)


class Compat(unittest.TestCase):
    """The compat check of the image mode, `--compat <dir> <variant>`, run
    here over a directory of the test's own: `lela/` stands in for the
    image's /usr/local/bin, its hosts under Lela's names, and `other/` for
    a php, php-cgi, php-fpm and phpdbg that are not the compat directory's."""

    USUAL = ("php", "php-cgi", "php-fpm", "phpdbg")
    LELA = {"php": "lela", "php-cgi": "lela-cgi", "php-fpm": "lela-fpm", "phpdbg": "lela-dbg"}

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.scratch = Path(temporary.name).resolve()
        self.compat = self.scratch / "compat"
        self.compat.mkdir()
        self.other = self.scratch / "other"
        self.lela = self.scratch / "lela"
        self.tools = tools(self.scratch / "tools")
        for directory, hosts in ((self.other, self.USUAL), (self.lela, self.LELA.values())):
            directory.mkdir()
            for host in hosts:
                program = directory / host
                program.write_text(f"#!/bin/sh\necho {directory.name} {host}\n")
                program.chmod(0o755)
        # The debugger runs a file and quits, and starts no prompt.
        (self.lela / "lela-dbg").write_text('#!/bin/sh\ncase "$*" in "-n -qrr "*) echo phpdbg ;; *) exit 1 ;; esac\n')

    def link(self, *usual):
        for host in usual:
            (self.compat / host).symlink_to(self.lela / self.LELA[host])

    def check(self, *path, variant="fpm"):
        return subprocess.run(
            [shutil.which("bash"), str(ROOT / "dist/smoke.sh"), "--compat", str(self.compat), variant],
            env={**os.environ, "NAME": "lela", "PATH": os.pathsep.join(map(str, [*path, self.lela, self.tools]))},
            capture_output=True, text=True,
        )

    def test_each_entry_is_what_its_name_finds_once_the_directory_is_first(self):
        self.link(*self.USUAL)
        run = self.check()
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn(f"compat: {self.compat}, not on PATH", run.stdout)
        self.assertIn("compat: no php, php-cgi, php-fpm or phpdbg on PATH", run.stdout)
        for host in self.USUAL:
            with self.subTest(host=host):
                self.assertIn(f"compat: {host} is {self.compat / host} with {self.compat} first on PATH", run.stdout)

    def test_a_directory_on_path_or_none_fails(self):
        self.link(*self.USUAL)
        run = self.check(self.compat)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} is on PATH", run.stderr)
        for host in self.USUAL:
            (self.compat / host).unlink()
        self.compat.rmdir()
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} is not a directory", run.stderr)

    def test_a_usual_name_on_path_fails(self):
        self.link(*self.USUAL)
        run = self.check(self.other)
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(
            f"compat: php is {self.other / 'php'} on PATH, where no usual name is until {self.compat} is put first",
            run.stderr,
        )

    def test_an_empty_directory_fails(self):
        run = self.check(variant="cli")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} holds no php", run.stderr)

    def test_an_entry_that_is_not_executable_fails(self):
        self.link("php-cgi", "php-fpm")
        (self.compat / "php").write_text("#!/bin/sh\necho compat php\n")
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat / 'php'} does not run", run.stderr)

    def test_an_entry_its_name_does_not_find_fails(self):
        # A directory, which the shell does not take for the name it finds.
        self.link("php", "php-fpm")
        (self.compat / "php-cgi").mkdir()
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: php-cgi is nothing with {self.compat} first on PATH", run.stderr)

    def test_an_entry_that_does_not_run_fails(self):
        self.link("php-cgi", "php-fpm")
        (self.compat / "php").symlink_to(self.scratch / "gone")
        run = self.check()
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn("compat:", run.stderr)

    def test_a_usual_name_the_variant_has_a_host_for_is_an_entry(self):
        self.link("php", "php-cgi")
        run = self.check(variant="cli")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat} holds no phpdbg", run.stderr)
        self.link("phpdbg")
        run = self.check(variant="cli")
        self.assertEqual(run.returncode, 0, run.stderr)
        for variant in ("fpm", "apache"):
            with self.subTest(variant=variant):
                run = self.check(variant=variant)
                self.assertEqual(run.returncode, 1, run.stdout)
                self.assertIn(f"compat: {self.compat} holds no php-fpm", run.stderr)

    def test_the_images_script_lays_each_usual_name_the_variant_has_a_host_for(self):
        """dist/image/compat.sh over a prefix laid as each image lays its
        hosts, run as the Dockerfile's stages run it, gives the directory
        the image's check passes: `phpdbg` in the cli image's alone."""
        for variant in publish.VARIANTS:
            with self.subTest(variant=variant):
                prefix = self.scratch / f"prefix-{variant}"
                for host in publish.in_image(variant):
                    path = prefix / host.path
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.symlink_to(self.lela / host.binary)
                compat = self.scratch / f"compat-{variant}"
                laid = subprocess.run(
                    ["/bin/sh", str(ROOT / "dist/image/compat.sh")],
                    env={**os.environ, "NAME": "lela", "COMPAT": str(compat), "PREFIX": str(prefix)},
                    capture_output=True, text=True,
                )
                self.assertEqual(laid.returncode, 0, laid.stderr)
                wanted = ["php", "php-cgi", "phpdbg"] if variant == "cli" else ["php", "php-cgi", "php-fpm"]
                self.assertEqual(sorted(entry.name for entry in compat.iterdir()), wanted)
                self.compat = compat
                run = self.check(variant=variant)
                self.assertEqual(run.returncode, 0, run.stderr)

    def test_the_debuggers_entry_runs_a_file(self):
        self.link("php", "php-cgi")
        (self.compat / "phpdbg").symlink_to(self.other / "phpdbg")
        (self.other / "phpdbg").write_text("#!/bin/sh\nexit 1\n")
        run = self.check(variant="cli")
        self.assertEqual(run.returncode, 1, run.stdout)
        self.assertIn(f"compat: {self.compat / 'phpdbg'} does not run", run.stderr)


if __name__ == "__main__":
    unittest.main()
