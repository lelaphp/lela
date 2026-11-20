"""Small offline tests for the production distribution verifier."""

import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
# The pinned php-src tree, where this machine has one.
TREE = Path(os.path.expanduser(os.environ.get("TEST_PHP_SRCDIR", "~/php-src/missing")))
spec =importlib.util.spec_from_file_location("inputs", ROOT / "dist/inputs.py")
inputs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inputs)


class DistributionInputs(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "dist").mkdir()
        for name in ("oracle.toml", "rust-toolchain.toml", "dist/inputs.toml"):
            shutil.copyfile(ROOT / name, self.root / name)
        self.oracle = inputs.read_toml(self.root / "oracle.toml")
        self.toolchain = inputs.read_toml(self.root / "rust-toolchain.toml")["toolchain"]

    def replace(self, name, old, new):
        path = self.root / name
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new))

    def cli(self, *arguments):
        # A download attempted during drift/architecture rejection would
        # fail to find curl: rejection must happen before any download.
        return subprocess.run(
            [sys.executable, str(ROOT / "dist/inputs.py"), "--root", str(self.root), *arguments],
            env={**os.environ, "PATH": ""},
            capture_output=True, text=True,
        )

    def test_checkout_alone_has_all_pins(self):
        self.assertEqual(self.cli("check").returncode, 0)
        pins = inputs.check(self.root)
        self.assertEqual(len(inputs.ini(pins)), 2)
        for _, url, digest in inputs.ini(pins):
            self.assertIn("/" + pins["php"]["commit"] + "/", url)
            self.assertRegex(digest, r"^[0-9a-f]{64}$")
        self.assertEqual(
            [(source, installed) for source, _, _, installed in inputs.fpm(pins)],
            [
                ("sapi/fpm/php-fpm.conf.in", "php-fpm.conf.default"),
                ("sapi/fpm/www.conf.in", "php-fpm.d/www.conf.default"),
            ],
        )
        for source, url, digest, _ in inputs.fpm(pins):
            self.assertTrue(url.endswith("/" + pins["php"]["commit"] + "/" + source), url)
            self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_fpm_templates_render_as_the_official_fpm_image_configures_php(self):
        # docker-library/php configures php with its default prefix,
        # --sysconfdir=/usr/local/etc and --with-fpm-user/group=www-data;
        # configure.ac and sapi/fpm/config.m4 derive the rest from those.
        template = (
            "include=@php_fpm_sysconfdir@/php-fpm.d/*.conf\n"
            "; prefix (@prefix@), @php_fpm_prefix@, @EXPANDED_LOCALSTATEDIR@\n"
            "; @EXPANDED_DATADIR@/fpm/status.html\n"
            "user = @php_fpm_user@\ngroup = @php_fpm_group@\n"
        )
        self.assertEqual(
            inputs.render(template),
            "include=/usr/local/etc/php-fpm.d/*.conf\n"
            "; prefix (/usr/local), NONE, /usr/local/var\n"
            "; /usr/local/share/php/fpm/status.html\n"
            "user = www-data\ngroup = www-data\n",
        )

    def test_a_placeholder_configure_would_not_fill_is_refused(self):
        with self.assertRaisesRegex(ValueError, "@EXPANDED_BINDIR@"):
            inputs.render("; @EXPANDED_BINDIR@/php\n")

    @unittest.skipUnless(
        (TREE / "sapi/fpm/www.conf.in").is_file(),
        "the pinned php-src tree is not on this machine (TEST_PHP_SRCDIR)",
    )
    def test_the_pinned_tree_s_fpm_templates_match_their_pins_and_render_whole(self):
        pins = inputs.check(self.root)
        for source, _, digest, _ in inputs.fpm(pins):
            with self.subTest(source=source):
                data = (TREE / source).read_bytes()
                self.assertEqual(hashlib.sha256(data).hexdigest(), digest)
                rendered = inputs.render(data.decode())
                self.assertNotRegex(rendered, r"@[A-Za-z_]+@")
        conf = inputs.render((TREE / "sapi/fpm/php-fpm.conf.in").read_text())
        self.assertIn("\ninclude=/usr/local/etc/php-fpm.d/*.conf\n", conf)
        pool = inputs.render((TREE / "sapi/fpm/www.conf.in").read_text())
        for line in ("user = www-data", "group = www-data", "listen = 127.0.0.1:9000"):
            self.assertIn("\n" + line + "\n", pool)

    def test_fpm_installs_each_rendered_template_where_make_install_puts_it(self):
        payloads = {
            "sapi/fpm/php-fpm.conf.in": b"include=@php_fpm_sysconfdir@/php-fpm.d/*.conf\n",
            "sapi/fpm/www.conf.in": b"user = @php_fpm_user@\n",
        }
        pins = inputs.check(self.root)
        pins["php"]["fpm"] = {name: hashlib.sha256(data).hexdigest() for name, data in payloads.items()}

        def curl(arguments, check):
            url = arguments[-1]
            source = next(name for name in payloads if url.endswith("/" + name))
            Path(arguments[arguments.index("--output") + 1]).write_bytes(payloads[source])

        out = self.root / "fpm"
        with patch.object(inputs.subprocess, "run", side_effect=curl):
            inputs.install_fpm(pins, out)
        self.assertEqual(
            (out / "php-fpm.conf.default").read_text(), "include=/usr/local/etc/php-fpm.d/*.conf\n"
        )
        self.assertEqual((out / "php-fpm.d/www.conf.default").read_text(), "user = www-data\n")
        self.assertEqual(
            sorted(str(path.relative_to(out)) for path in out.rglob("*")),
            ["php-fpm.conf.default", "php-fpm.d", "php-fpm.d/www.conf.default"],
        )

    def test_external_docker_images_are_digest_pinned(self):
        dockerfile = (ROOT / "dist/Dockerfile").read_text()
        self.assertRegex(dockerfile.splitlines()[0], r"^# syntax=docker/dockerfile:[^@]+@sha256:[0-9a-f]{64}$")
        arguments = {}
        stages = {"scratch"}
        external = []
        for line in dockerfile.splitlines():
            if line.startswith("ARG ") and "=" in line:
                name, value = line[4:].split("=", 1)
                arguments["${" + name + "}"] = value
            if line.startswith("FROM "):
                words = line.split()
                image = words[2] if words[1].startswith("--platform=") else words[1]
                image = arguments.get(image, image)
                if image not in stages:
                    external.append(image)
                    self.assertRegex(image, r"@sha256:[0-9a-f]{64}$")
                stages.add(words[-1])
        self.assertTrue(external)

    def test_oracle_commit_tag_and_version_drift_stop_before_download(self):
        for old, new in (
            (f'commit = "{self.oracle["source"]["commit"]}"', 'commit = "00000000"'),
            (f'tag = "{self.oracle["source"]["tag"]}"', 'tag = "php-0.0.0"'),
            (f'version = "{self.oracle["oracle"]["version"]}"', 'version = "0.0.0"'),
        ):
            with self.subTest(field=old):
                original = (self.root / "oracle.toml").read_text()
                self.replace("oracle.toml", old, new)
                run = self.cli("ini", str(self.root / "out"))
                self.assertEqual(run.returncode, 1)
                self.assertIn("oracle.toml source/version changed", run.stderr)
                self.assertFalse((self.root / "out").exists())
                (self.root / "oracle.toml").write_text(original)

    def test_full_oracle_commit_is_accepted_but_short_or_empty_is_not(self):
        full = inputs.check(self.root)["php"]["commit"]
        current = self.oracle["source"]["commit"]
        self.replace("oracle.toml", f'commit = "{current}"', f'commit = "{full}"')
        self.assertEqual(self.cli("check").returncode, 0)
        different = full[:-1] + ("1" if full[-1] == "0" else "0")
        for commit in ("", full[:4], different):
            with self.subTest(commit=commit):
                original = (self.root / "oracle.toml").read_text()
                self.replace("oracle.toml", f'commit = "{full}"', f'commit = "{commit}"')
                self.assertNotEqual(self.cli("check").returncode, 0)
                (self.root / "oracle.toml").write_text(original)

    def test_abbreviated_distribution_commit_is_rejected(self):
        full = inputs.check(self.root)["php"]["commit"]
        self.replace("dist/inputs.toml", full, full[:8])
        run = self.cli("check")
        self.assertEqual(run.returncode, 1)
        self.assertIn("must be a full commit", run.stderr)

    def test_toolchain_channel_and_components_drift_stop_before_download(self):
        original = (self.root / "rust-toolchain.toml").read_text()
        for old, new in (
            (self.toolchain["channel"], "0.0.0"),
            (f'"{self.toolchain["components"][0]}"', '"dist-test-component"'),
        ):
            with self.subTest(field=old):
                self.replace("rust-toolchain.toml", old, new)
                run = self.cli("rustup", "arm64", str(self.root / "rustup-init"))
                self.assertEqual(run.returncode, 1)
                self.assertIn("rust-toolchain.toml changed", run.stderr)
                self.assertFalse((self.root / "rustup-init").exists())
                (self.root / "rust-toolchain.toml").write_text(original)

    def test_build_architecture_selects_published_installer(self):
        pins = inputs.check(self.root)
        for arch, host in (("amd64", "x86_64"), ("arm64", "aarch64")):
            with self.subTest(arch=arch):
                url, digest = inputs.rustup(pins, arch)
                self.assertEqual(
                    url, "https://static.rust-lang.org/rustup/archive/"
                    + pins["rustup"]["version"] + "/" + host + "-unknown-linux-gnu/rustup-init",
                )
                self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_unsupported_build_architecture_fails_before_download(self):
        run = self.cli("rustup", "riscv64", str(self.root / "rustup-init"))
        self.assertEqual(run.returncode, 1)
        self.assertIn("unsupported BUILDARCH: riscv64", run.stderr)

    def downloader(self, payload, failure=False):
        def curl(arguments, check):
            self.assertTrue(check)
            self.assertIn("--fail", arguments)
            self.assertIn("--location", arguments)
            self.assertEqual(arguments[arguments.index("--proto") + 1], "=https")
            self.assertEqual(arguments[arguments.index("--proto-redir") + 1], "=https")
            Path(arguments[arguments.index("--output") + 1]).write_bytes(payload)
            if failure:
                raise subprocess.CalledProcessError(22, arguments)
        return patch.object(inputs.subprocess, "run", side_effect=curl)

    def test_verified_bytes_reach_destination(self):
        payload = b"verified distribution input\n"
        destination = self.root / "out" / "rustup-init"
        with self.downloader(payload):
            inputs.download("https://example.invalid/input", hashlib.sha256(payload).hexdigest(), destination)
        self.assertEqual(destination.read_bytes(), payload)
        self.assertEqual(list(destination.parent.iterdir()), [destination])

    def test_changed_byte_of_each_download_is_rejected(self):
        pins = inputs.check(self.root)
        downloads = [(name, url, digest) for name, url, digest in inputs.ini(pins)]
        downloads += [(arch, *inputs.rustup(pins, arch)) for arch in ("amd64", "arm64")]
        downloads += [(Path(source).name, url, digest) for source, url, digest, _ in inputs.fpm(pins)]
        for name, url, _ in downloads:
            with self.subTest(input=name):
                payload = b"original bytes\n"
                destination = self.root / name
                # A previous verified file must survive an interrupted or bad refresh.
                destination.write_bytes(payload)
                digest = hashlib.sha256(payload).hexdigest()
                with self.downloader(b"Original bytes\n"):
                    with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                        inputs.download(url, digest, destination)
                self.assertEqual(destination.read_bytes(), payload)

    def test_failed_download_leaves_no_input(self):
        destination = self.root / "out" / "php.ini-production"
        with self.downloader(b"incomplete", failure=True):
            with self.assertRaises(subprocess.CalledProcessError):
                inputs.download("https://example.invalid/input", "0" * 64, destination)
        self.assertFalse(destination.exists())
        self.assertEqual(list(destination.parent.iterdir()), [])

    def test_invalid_digest_is_rejected_before_network(self):
        with patch.object(inputs.subprocess, "run") as curl:
            with self.assertRaisesRegex(ValueError, "invalid SHA-256 pin"):
                inputs.download("https://example.invalid/input", "no pin", self.root / "out")
        curl.assert_not_called()


def stages(text):
    """Each stage of a Dockerfile by its name: its base and its
    instructions, a continued line joined and a comment line dropped."""
    instructions, pending = [], ""
    for line in text.splitlines():
        if line.strip().startswith("#") or not line.strip():
            continue
        if line.endswith("\\"):
            pending += line[:-1] + " "
            continue
        keyword, _, rest = (pending + line).strip().partition(" ")
        instructions.append((keyword.upper(), rest.strip()))
        pending = ""
    found, stage = {}, None
    for keyword, rest in instructions:
        if keyword == "FROM":
            words = [word for word in rest.split() if not word.startswith("--platform=")]
            stage = found.setdefault(words[-1], {"from": words[0], "instructions": []})
        elif stage is not None:
            stage["instructions"].append((keyword, rest))
    return found


# docker-library/php's docker-php-entrypoint as its apply-templates.sh
# copies it into the cli variant, before it renders the others.
OFFICIAL_ENTRYPOINT = """#!/bin/sh
set -e

# first arg is `-f` or `--some-option`
if [ "${1#-}" != "$1" ]; then
\tset -- php "$@"
fi

exec "$@"
"""


class ImageTargets(unittest.TestCase):
    """What the cli, fpm and apache targets copy from docker-library/php's
    Dockerfile-linux.template and apply-templates.sh."""

    # Each variant's command, working directory, port and stop signal, as
    # the template and apply-templates.sh give them; None where it sets none.
    OFFICIAL = {
        "cli": (["php", "-a"], None, None, None),
        "fpm": (["php-fpm"], "/var/www/html", "9000", "SIGQUIT"),
        "apache": (["apache2-foreground"], "/var/www/html", "80", "SIGWINCH"),
    }

    def setUp(self):
        text = (ROOT / "dist/Dockerfile").read_text()
        self.stages = stages(text)
        (self.name,) = [line.split("=", 1)[1] for line in text.splitlines() if line.startswith("ARG NAME=")]

    def renamed(self, word, name=None):
        """The official template's word with its hosts under Lela's names,
        `name` for the Dockerfile's `ARG NAME` (ADR-0016)."""
        name = name or self.name
        return name + word.removeprefix("php") if word in ("php", "php-cgi", "php-fpm") else word

    def chain(self, name):
        """A stage's instructions after those of the stages it is built on."""
        stage = self.stages[name]
        before = self.chain(stage["from"]) if stage["from"] in self.stages else []
        return before + stage["instructions"]

    def last(self, name, keyword):
        values = [rest for word, rest in self.chain(name) if word == keyword]
        return values[-1] if values else None

    def test_the_three_variants_share_one_base_and_one_compile(self):
        self.assertEqual({self.stages[variant]["from"] for variant in self.OFFICIAL}, {"base"})
        compiles = [
            name for name, stage in self.stages.items()
            for keyword, rest in stage["instructions"] if keyword == "RUN" and "cargo build" in rest
        ]
        self.assertEqual(compiles, ["build"])
        for variant in self.OFFICIAL:
            with self.subTest(variant=variant):
                binaries = [
                    rest for keyword, rest in self.chain(variant)
                    if keyword == "COPY" and rest.startswith("--from=build /out/usr/local/")
                ]
                self.assertTrue(binaries)

    def test_the_cli_holds_no_fpm_and_the_others_do(self):
        for variant, fpm in (("cli", False), ("fpm", True), ("apache", True)):
            with self.subTest(variant=variant):
                copies = " ".join(rest for keyword, rest in self.chain(variant) if keyword == "COPY")
                self.assertIn("--from=build /out/usr/local/bin/ ", copies)
                self.assertEqual("--from=build /out/usr/local/sbin/ " in copies, fpm)

    def test_each_variant_has_the_official_entrypoint_command_directory_port_and_signal(self):
        for variant, (command, directory, port, signal) in self.OFFICIAL.items():
            with self.subTest(variant=variant):
                self.assertEqual(self.last(variant, "ENTRYPOINT"), '["docker-php-entrypoint"]')
                self.assertEqual(json.loads(self.last(variant, "CMD")), [self.renamed(w) for w in command])
                self.assertEqual(self.last(variant, "WORKDIR"), directory)
                self.assertEqual(self.last(variant, "EXPOSE"), port)
                self.assertEqual(self.last(variant, "STOPSIGNAL"), signal)

    def test_the_entrypoint_is_rendered_for_each_variant_as_apply_templates_renders_it(self):
        entrypoint = (ROOT / "dist/image/bin/docker-php-entrypoint").read_text()
        official = 'set -- php "$@"'
        self.assertIn(official, OFFICIAL_ENTRYPOINT)
        self.assertEqual(entrypoint, OFFICIAL_ENTRYPOINT.replace(official, f'set -- {self.name} "$@"'))
        for variant, (command, *_) in self.OFFICIAL.items():
            with self.subTest(variant=variant):
                runs = " ".join(rest for keyword, rest in self.stages[variant]["instructions"] if keyword == "RUN")
                sed = f'sed -i -e "s! $NAME ! {self.renamed(command[0], "$NAME")} !g" /usr/local/bin/docker-php-entrypoint'
                self.assertEqual(sed in runs, command[0] != "php")

    def test_the_ini_location_is_the_official_image_s(self):
        text = (ROOT / "dist/Dockerfile").read_text()
        self.assertIn("\nARG PHP_INI_DIR=/usr/local/etc/php\n", text)
        base = self.stages["base"]["instructions"]
        self.assertTrue(any(k == "ENV" and "PHP_INI_DIR=${PHP_INI_DIR}" in rest for k, rest in base))
        self.assertTrue(any(k == "RUN" and 'mkdir -p "$PHP_INI_DIR/conf.d"' in rest for k, rest in base))
        self.assertIn(("COPY", "--from=ini /ini/ ${PHP_INI_DIR}/"), base)

    def test_the_fpm_target_configures_php_fpm_as_the_official_image_does(self):
        fpm = self.stages["fpm"]["instructions"]
        self.assertIn(("COPY", "--from=fpm-config /fpm/ /usr/local/etc/"), fpm)
        self.assertEqual(self.stages["fpm-config"]["from"], "inputs")
        runs = " ".join(rest for keyword, rest in fpm if keyword == "RUN")
        for line in (
            "cp -v php-fpm.conf.default php-fpm.conf;",
            "cp -v php-fpm.d/www.conf.default php-fpm.d/www.conf;",
            "sed -ri 's/^(listen = 127.0.0.1:9000)/;\\1/' php-fpm.d/www.conf;",
            "echo 'listen = 9000';",
            "} | tee php-fpm.d/docker.conf;",
            "echo 'daemonize = no';",
            "} | tee php-fpm.d/zz-docker.conf;",
            "echo 'fastcgi.logging = Off';",
            '} > "$PHP_INI_DIR/conf.d/docker-fpm.ini"',
        ):
            self.assertIn(line, runs)

    def test_the_compat_directory_is_off_path_under_lela_s_prefix(self):
        text = (ROOT / "dist/Dockerfile").read_text()
        compat = [line.split("=", 1)[1] for line in text.splitlines() if line.startswith("ARG COMPAT=")]
        self.assertEqual(compat, ["/opt/lela/compat"])
        for name, stage in self.stages.items():
            for keyword, rest in stage["instructions"]:
                if keyword == "ENV" and "PATH=" in rest:
                    self.assertNotIn("COMPAT", rest, name)
                    self.assertNotIn(compat[0], rest, name)


if __name__ == "__main__":
    unittest.main()
