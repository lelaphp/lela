# Distribution inputs

Build with the checkout as the context, without a php-src checkout:

```sh
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target binaries --output /tmp/dist-arm64 .
docker buildx build -f dist/Dockerfile --platform linux/amd64 --target binaries --output /tmp/dist-amd64 .
```

Run those builds sequentially on small builders. The default Cargo job count is
one. The installer runs on BUILDARCH; the compiled hosts and their libraries
follow TARGETARCH. Product naming remains governed by ADR-0016.

## The images

Three targets stand in for the variants of php's official image, each
shaped as docker-library/php's templates shape it: the same
`docker-php-entrypoint`, working directory and `$PHP_INI_DIR`
(`/usr/local/etc/php`, with `conf.d` in it).

| target | holds | default command | working directory |
|---|---|---|---|
| `cli` | the CLI, CGI and debugger hosts, `lela`, `lela-cgi` and `lela-dbg` | `lela -a` | `/` |
| `fpm` | the CLI, CGI and FPM hosts, `lela`, `lela-cgi` and `lela-fpm`, the FPM listening on port 9000 | `lela-fpm` | `/var/www/html` |
| `apache` | those three, and Apache's prefork MPM in front of the FPM over a unix socket | `apache2-foreground` | `/var/www/html` |

A release carries the debugger host as php does, as Mitchell ruled on #79
on 2026-11-21: every tarball holds all four hosts, the `cli` image holds
`lela-dbg`, and the `fpm` and `apache` images do not. Where each host
ships is the harness's one host table, which `cargo xtask hosts --list`
prints, a line of JSON per host naming its binary, crate, package, whether
the tarballs hold it and the images that do, and builds nothing. dist/
keeps one copy of it, `HOSTS` in `dist/publish.py`, since the jobs that
publish install no Rust toolchain; a test holds that copy to the command
wherever cargo is, and every other list of hosts here, the Dockerfile's
cargo line and what each of its stages holds, the release's tarballs and
the smoke's, to the copy.

```sh
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target cli -t lela:cli --load .
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target fpm -t lela:fpm --load .
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target apache -t lela:apache --load .
```

All three are built on one `base` stage from one `build` stage: the hosts
compile once per platform, and the second and third target find that
compile cached. They share the Ubuntu packages the build stage reads from
the hosts' ELF files, and the compat directory, `/opt/lela/compat`, which
no image puts on `PATH`. The hosts carry Lela's names (ADR-0016), `lela`,
`lela-cgi`, `lela-fpm` and `lela-dbg`, which the images' own scripts and
default commands run, and no `php` is on an image's `PATH`. The compat
directory holds `php`, `php-cgi`, `php-fpm` and `phpdbg` where the image
holds the host, each a symlink to the host under Lela's name: `phpdbg` in
the `cli` image, `php-fpm` in the other two. One line in a Dockerfile
built on the image opts in:

```dockerfile
ENV PATH=/opt/lela/compat:$PATH
```

The `fpm` image's `php-fpm.conf` and `php-fpm.d/www.conf` start from
php-src's templates at the pinned commit, rendered as the official image's
`configure` renders them (`dist/inputs.py fpm`), and its `docker.conf`,
`zz-docker.conf` and `docker-fpm.ini` are the template's.

### The smoke over an image

`dist/smoke.sh --image <image> <variant>` runs the tarball's checks inside
the image, on the image's own platform, over the hosts in `/usr/local`:
that they are the variant's, `lela-dbg` in `cli` alone and `lela-fpm` in
`fpm` and `apache` alone, and no other host of Lela's name, then each
one's own checks, `lela-fpm -t` and `lela-dbg -qrr` on a file among them.
It checks the ini directory; that no `php`, `php-cgi`, `php-fpm` or
`phpdbg` is on `PATH`; that the compat directory is there, off `PATH`,
and holds each usual name the
variant has a host for; and that each entry of it is what its name finds
once the directory is first on `PATH`, and runs: that makes putting it
first the only step the usual names need. Then it checks what a user
of the variant sees: `cli` passes an option to `lela` through the
entrypoint; `fpm` answers a FastCGI request on port 9000, and `apache`
serves a page on port 80, each to a second container of the image on a
network of the smoke's own. It prints the platform it ran on, saying when
Docker emulates it, and exits nonzero at the first wrong answer:

```sh
dist/smoke.sh --image lela:cli cli
dist/smoke.sh --image lela:fpm fpm
dist/smoke.sh --image lela:apache apache
```

### Publishing the images

`dist/publish.py` publishes one ref's images to `ghcr.io/lelaphp/lela`, in
two steps, so that each platform builds on a machine of its own
architecture. The ref is `main` or a release's tag, `v<version>`:

```sh
dist/publish.py build main --digests /tmp/digests-arm64
dist/publish.py join main /tmp/digests-amd64 /tmp/digests-arm64
```

- `build` builds the three variants for the daemon's platform, which
  `--platform` may name, and loads each; runs the smoke's image mode over
  each; and only once all three have passed, pushes each by digest with
  BuildKit's SBOM attached. A failed smoke pushes nothing. Each pushed
  image is held to the image smoked by its manifest digest, which covers
  the config, the layers as compressed and the manifest's annotations:
  the smoked one is the digest the loaded build's metadata names, its
  `containerimage.descriptor` and `containerimage.digest`, which a load
  into a containerd store names too, where it names no config. Each push
  is an image index, so the pushed one is read from the index the
  registry holds, its manifest for the platform and never its
  attestation manifest, and a push whose manifest is not the smoked one
  is refused, its digest named nowhere. A load that is an index, as the
  docker driver's into a containerd store is, its provenance beside the
  image, is refused before its smoke; a docker-container builder, the
  one the workflow creates, loads the image's manifest alone. A real build
  refuses a platform the daemon does not run as its own, as
  `dist/release.py build` does, so no image smoked under emulation is
  pushed; the smoke alone still runs one. It writes one
  `<platform> <variant> <digest>` line per variant.
- `join` reads both platforms' lines and puts each variant's two images
  under one manifest list, carrying every tag of the variant:

  | ref | tags |
  |---|---|
  | `main` | `nightly-<variant>`, which moves on every merge to `main`, not once a night: the word names the channel, not a schedule; and `sha-<commit>-<variant>` with the full commit |
  | `v<version>` | `<version>-<variant>`, and the bare `<variant>` unless the version is a pre-release or older than a release tag the checkout holds |

  Whether a release is the newest is read from the checkout's release
  tags, so a shallow checkout, which may hold its own tag alone and would
  move the bare variant for every backport, is refused, saying to fetch
  the whole history and every tag (`git fetch --unshallow --tags`,
  actions/checkout's `fetch-depth: 0`). A real `join` refuses a tree with
  changes and a release whose tag is not HEAD, as a real `build` does.
  It writes one `<variant> <digest>` line per list, the digest the sha256
  of the bytes the registry holds at the list's tags, read back with
  `imagetools inspect <tag> --raw`: tags that name two lists, or a list
  that is no image index, are refused. No buildx before v0.32.0 writes
  what `imagetools create` pushed anywhere a script reads, and the CI
  runners' buildx is older.

A real step of either kind first prints `buildx <version>`, what `docker
buildx version` says, so a run's log names the buildx that answered. The
CI runners' buildx is not the one a laptop has: run 44731512379's refused
`imagetools create --metadata-file`, and its usage line and build output
are those of a buildx from v0.14.0 to v0.23.0. Each flag and format the steps and the images
workflow pass is one buildx v0.14.0 knows, the oldest they serve:

| what | buildx since |
|---|---|
| `build --metadata-file`, its `containerimage.descriptor` | v0.6.0, v0.8.0 |
| `build --sbom=true`, and the default provenance | v0.10.0 |
| `imagetools create --annotation index:…` | v0.12.0 |
| `imagetools inspect --raw` | v0.4.0 or before |
| `imagetools inspect --format '{{json .SBOM}}'`, `'{{json .Provenance}}'` | v0.10.0 |

`imagetools create --metadata-file`, v0.32.0, is the one it may not pass.

Each image is labelled, and each list annotated, with standard OCI keys
alone: `source`, `revision`, `version` (`nightly` from `main`) and
`licenses`, and three that replace the Ubuntu base's, since a build can
replace a label and cannot remove one: `description` and `title`, `Lela`, and `created`, the
commit's time in RFC 3339, UTC, the instant the build's
`SOURCE_DATE_EPOCH` names. `licenses` is one SPDX expression per image, the
union of the `license` lines of the workspace's own crates that image's
hosts link, as Mitchell ruled on #14 on 2026-11-20, so the `cli` image's
counts `lela-dbg`'s crates and the `fpm` and `apache` images' count
`lela-fpm`'s: the workspace manifest's
line first, then each other term once, an identifier by name and a choice
in parentheses, such as `(MIT OR NCSA)`, kept whole. The crates are read
from the manifests with `tomllib`, since the images workflow installs no
Rust toolchain: from each host of the image, with the features
`dist/Dockerfile`'s one cargo line unifies across the four hosts it
builds with their defaults, over its normal dependencies on the workspace's crates, an
optional one where a feature turned on names it, and a target table
where its `cfg` holds for `linux/amd64` or `linux/arm64`. A test holds
that reading to `cargo metadata` where cargo is. Every term is an
identifier of the SPDX License List the script names, or a
`LicenseRef-`; a crate or a workspace manifest with no `license` line, or
a term that is neither, is refused by name, and nothing runs. Third
parties' crates and the system's C libraries are the SBOM's. A release's
tag is held to the workspace manifest's version by both steps, dry or
real, through the hold `dist/release.py` uses: a tag that names another
version is refused, and so is a manifest with no version line, as this
tree's is until the version line lands. `--dry-run` prints the platform,
the labels, the tags and the digests each step would push, and pushes
nothing:

```sh
dist/publish.py build v0.1.0-alpha.1 --platform linux/amd64 --dry-run
dist/publish.py join v0.1.0-alpha.1 --dry-run
```

`dist/prune.py` deletes the versions no tag is pulled by: one last updated
more than 30 days ago whose tags are all `sha-` tags, or which has none,
unless a kept manifest list names it or it is a referrer of a kept one.
It also deletes, however young, every list under an `edge-` tag, the name
`nightly-` replaced, so that no version keeps both names. GHCR deletes a
version and not one of its tags, so the `sha-` tag that shares an `edge-`
list's version goes with it, before its 30 days are up; the list's images
and attestations, untagged and named by nothing kept, go once they are
older than 30 days. A version tag, a bare variant, a `nightly-` tag and a
tag of any other kind are never deleted. `dist/prune.py --dry-run` names each version it would
delete, and deletes none. A listing gh cannot give deletes nothing, and a
deletion that fails stops the run, naming the versions deleted before it
and those not. CI runs both scripts; a person runs them with
`gh` authorised for the organisation's packages and docker logged in to
`ghcr.io`.

## A release

CI builds the release, from its tag, and nothing of it is built by hand. A
person's acts are two:

1. **The version bump.** The version is one line in the workspace
   manifest's package table, `version = "<version>"` under
   `[workspace.package]` in `Cargo.toml`, which the host crates inherit
   and no other crate does. The bump is a commit of that line.
2. **The tag.** `v<version>` on that commit, pushed.

The tag's workflow then runs `dist/release.py`, whose every step first
holds the tag to the manifest's version: a tag that names another version
is refused, and so is a manifest with no version line, saying the line it
lacks. It builds each platform's tarball on a runner of that platform, and
the images through `dist/publish.py`'s release path, the one a commit on
`main` takes, before it creates the GitHub Release:

```sh
dist/release.py check v0.1.0-alpha.1
dist/release.py build v0.1.0-alpha.1 linux-x86_64 /tmp/release
dist/release.py publish v0.1.0-alpha.1 /tmp/release
```

- `check` says the version, and refuses as each step does.
- `build` builds one platform's tarball, `<name>-<version>-<platform>.tar.gz`
  holding one directory of that name, on a machine of that platform
  alone: `darwin-arm64` with cargo on an Apple silicon Mac, and
  `linux-aarch64` and `linux-x86_64` from the `binaries` target above on a
  daemon of that architecture. It then smokes the tarball itself, unpacked:
  `dist/smoke.sh` holds its `bin/` and `sbin/` to the four hosts and no
  other, then runs `-v`, `-r`, a file with arguments, code on stdin,
  `lela-cgi` on a file, `lela-fpm -t` and `lela-dbg -qrr` on a file, a
  Linux tarball in the Ubuntu image
  the Dockerfile builds on with its `runtime-packages.txt` installed. Then
  the release's `install.sh` installs the tarball there as a user runs it,
  ending with `lela -v` through its link: in that image as a user of its
  own, curl installed once the smoke has passed, and on the Mac into a
  home of its own on the system's `PATH` alone. A
  macOS tarball carries its libraries, below, and each Mach-O in it an
  ad-hoc signature, which `build` reads before the smoke, with no
  Developer ID and no notarization.
- `publish` takes the tarball of each platform from the directory,
  refusing a missing one or one of another version, stages `install.sh`
  (below) and `platforms.json` beside them, writes `SHA256SUMS` over every
  file staged with `scripts/dist-sums.sh`, and creates the GitHub Release
  of the tag with them and the sums attached. The release is marked a
  pre-release when the version is one, and the latest when its images move
  the bare variant tags, which it reads as `join` does, refusing a shallow
  checkout.

The platforms are one table, `PLATFORMS` in `dist/release.py`: a row per
platform with its name, the Docker platform a Linux tarball is built for,
the `uname -s` and `uname -m` words of a machine of it, the checks of
`install.sh` it is held to (`ubuntu`, `macos`), and whether `install.sh`
installs on it. A row that installs held to checks the script has not is
refused by `build` and `publish`, their dry runs included: a musl Linux
is Linux to `uname -s`, so its row is refused when the table renders, not
by the Ubuntu checks on a user's machine. A build is asked for a platform
of the table and no other, `publish` takes the table's tarballs, and `platforms.json`
and the installer's platforms are written from it, so a platform added to
the table is in each of them, and a release's reader takes its
platforms from that file and not from the installer's text:

```json
{
  "version": "0.1.0-alpha.1",
  "platforms": [
    {
      "name": "darwin-arm64",
      "tarball": "lela-0.1.0-alpha.1-darwin-arm64.tar.gz",
      "installs": true
    },
    ...
  ]
}
```

An Intel Mac is a refusal of the installer, not a platform, so it is no
row of the table and none of `platforms.json`.

The release workflow's build matrix is printed from the table too, so a
platform is added to `PLATFORMS` and nowhere in the workflow:

```sh
dist/release.py matrix
```

It prints one line of JSON, which the workflow's `check` job writes to
its outputs and each build job reads with `fromJSON`: the table's
platforms in its order, whose tarballs the `release` job lays down, and
under `families` a matrix for each family's job, `darwin` for the
platforms cargo builds on a Mac and `linux` for those the Dockerfile
builds, a row per platform with its name and its Docker platform. Every
family is printed, one with no row too. It takes no tag and holds
nothing. A row's runner stays the workflow's to choose, by its family
and its Docker platform, since the runners are the repository's
variables and not the release's:

```json
{"platforms": ["darwin-arm64", "linux-aarch64", "linux-x86_64"], "families": {"darwin": {"include": [{"platform": "darwin-arm64", "docker": null}]}, "linux": {"include": [...]}}}
```

A release attaches the tarballs, `install.sh`, `platforms.json` and
`SHA256SUMS`, which names every other file the release attaches, the
installer included, in the format `sha256sum -c` reads.

A real `build` or `publish` refuses a tree with changes, and a tag that is
not HEAD. `--dry-run` prints what the step would make and makes nothing:
`build` the platform, the commit, the tarball and its smoke; `publish` the
artifacts, the release's fields, `platforms.json` as it would be written,
a `platforms.json` line per line of the file, the installer's platforms,
an `install.sh` line per machine, and the image tags
`dist/publish.py` would push, which a pre-release's are without a bare
variant:

```sh
dist/release.py publish v0.1.0-alpha.1 --dry-run
```

The sums read back in the directory they are written in:

```sh
(cd /tmp/release && shasum -a 256 -c SHA256SUMS)
```

Docker Desktop's default VM (about 4 GiB) links one host at a time, so
`CARGO_BUILD_JOBS` stays at its default of one.

The hosts' name is the Dockerfile's `ARG NAME`, `lela` (ADR-0016), which
xtask's tests hold to `php_engine::PRODUCT_NAME`, the constant the
`[[bin]]` names of the hosts are spelled from; no caller sets it. A
tarball, `lela-<version>-<platform>.tar.gz`, holds one directory of that
name with the four hosts, `bin/lela`, `bin/lela-cgi`, `sbin/lela-fpm`
and `bin/lela-dbg`, then `etc/php.ini-development` and
`etc/php.ini-production`, and a Linux one `runtime-packages.txt` beside
them. No tarball holds a `php`.

### The macOS tarball's libraries

The macOS hosts link libraries of Homebrew's kegs, which a Mac that runs
the tarball need not have, so the tarball carries them (#16, #33).
`dist/bundle.py` reads each host with `otool -l` and follows every library
it loads outside `/usr/lib` and `/System`, by an absolute path,
`@loader_path`, `@executable_path` or `@rpath`, and every library those
load, as dyld would. It copies each library's real file into `lib/` under
the file name of its own install name, rewrites each name to
`@loader_path/../lib/<name>` in a host and `@loader_path/<name>` in a
library, makes a library's own install name `@rpath/<name>`, deletes every
run path that is not the system's, and signs each rewritten file again ad
hoc, since arm64 refuses one whose signature no longer matches. No path of
the build machine is left in a name, so the directory runs from wherever it
is unpacked or moved to.

```
lela-<version>-darwin-arm64/
  bin/lela  bin/lela-cgi  bin/lela-dbg  sbin/lela-fpm
  etc/php.ini-development  etc/php.ini-production
  lib/<each library>
  licenses/<keg>/<each licence file of the keg>
  bundled-libraries.txt
```

`bundled-libraries.txt` says `macos <version>` on its first line, the
oldest macOS the tarball runs on, the highest `LC_BUILD_VERSION` minimum
of any Mach-O in it, which the build runner's bottles set; each line after
is `lib/<name> <keg> <version>`. A library in no keg
(`<prefix>/Cellar/<keg>/<version>/`), a keg with no `LICENSE*`, `LICENCE*`,
`COPYING*` or `NOTICE*` file, a name found nowhere and two libraries of
one file name are each refused by name. The staging reads the stage again
once it has rewritten it, and refuses a name still outside it and the
system's; `dist/smoke.sh` holds the unpacked tarball to the same with
`otool -L`, `otool -l` and `codesign --verify` (`dist/smoke.sh --machos
<dir>` alone), then runs the hosts in a sandbox that denies reading
Homebrew's prefix, where `brew` and `sandbox-exec` are on the machine.
`dist/release.py build ... darwin-arm64 ... --dry-run` names the libraries
the hosts in `target/release` load, where all three are built.

The program to put on `PATH` is `bin/lela` of the unpacked directory, by a
link to it: a copy of `bin/lela` alone finds no `../lib`. dyld resolves
`@loader_path` from the real file the link names, so the link runs.

### The paths the bundled libraries compile in

The rewrite reaches load commands, install names and run paths, so a path a
library compiles in stays the build machine's: three libraries under `lib/`
name Homebrew's prefix (#36). What each does on a Mac without it was probed
on a laid tarball tree with `sandbox-exec -p '(version 1)(allow
default)(deny file-read* (subpath "/opt/homebrew"))'`, beside the same tree
unsandboxed and the oracle:

- `libcrypto.4.dylib` compiles in OPENSSLDIR `/opt/homebrew/etc/openssl@4`,
  where its `cert.pem` and `openssl.cnf` are, and MODULESDIR, where the
  provider modules are. An https stream that verifies its peer, given no CA
  by `openssl.cafile`, `openssl.capath`, its `cafile` or `capath` option,
  `SSL_CERT_FILE` or `SSL_CERT_DIR`, trusts macOS's `/etc/ssl/cert.pem`
  where the compiled-in `cert.pem` cannot be read, as #35 ruled: ext/openssl
  does it on macOS, so an unpacked tarball needs no installer, ini or
  environment for it, and no CA bundle is shipped. Where the file can be
  read nothing changes. `dist/smoke.sh` holds it under the sandbox: a
  stream to `https://www.php.net/` reads and one to
  `https://expired.badssl.com/` is refused, each tried only where curl,
  outside the sandbox, reaches the host, and its line says so where it does
  not. The functions that verify outside a stream,
  `openssl_x509_checkpurpose`, `openssl_pkcs7_verify` and
  `openssl_cms_verify`, still read only the compiled-in defaults, which
  #47 asks a person about, and `openssl_get_cert_locations()` names
  Homebrew's paths, as php's does. With no `openssl.cnf` to read,
  `openssl_pkey_new()` and `openssl_csr_new()` without a `config` option
  return false, as the oracle's do with `OPENSSL_CONF` naming no file.
  `OPENSSL_CONF=/etc/ssl/openssl.cnf` loads, but macOS's file leaves
  `default_bits` commented out, so `openssl_pkey_new()` with no
  `private_key_bits` still warns "Private key length must be at least 384
  bits, configured to 0" and returns false, as the oracle does with that
  file; with `private_key_bits` both calls succeed. #46 asks a person which
  file the tarball reads. The cipher list is the same in and out of the
  sandbox: php loads no provider module unless built with
  `--with-openssl-legacy-provider` or a configuration activates one, and
  the default provider is built into the library.
- `libfontconfig.1.dylib` compiles in `/opt/homebrew/etc/fonts`,
  `/opt/homebrew/var/cache/fontconfig` and the keg's `conf.avail`, and no
  host's output depends on them: php never asks libgd to look a font up
  through fontconfig, and libgd finds a font by name in `GDFONTPATH`, else
  `/usr/share/fonts/truetype:/System/Library/Fonts:/Library/Fonts`.
  `imagettftext()` and `imagettfbbox()` with a font's path, with `Geneva`,
  `Arial` and a name of no font, and `gd_info()`, give the same answers in
  and out of the sandbox and on the oracle, and `FC_DEBUG=1031`, which
  makes fontconfig print the files it loads, prints nothing during them.
- `libintl.8.dylib` compiles in `/opt/homebrew/Cellar/gettext/1.0/share/locale`
  as the directory of a domain nothing has bound, which gettext's keg does
  not install, so it is absent with Homebrew present too.
  `bindtextdomain($domain, null)` answers that path, in and out of the
  sandbox, as the oracle does, and `gettext()`, `dgettext()` and
  `dngettext()` under `de_DE.UTF-8` find no catalog there in any of the
  three.

### The installer

Each release carries `install.sh`, which installs that release for the
user who runs it (#20):

```sh
curl -fsSL https://github.com/lelaphp/lela/releases/download/v<version>/install.sh | sh
```

`dist/install.sh` is its source, POSIX `sh`, and `publish` bakes the
release into its five empty lines: the hosts' name, the version, the
Ubuntu release of the Dockerfile's `ARG UBUNTU`, the URL of the
release's assets, and its platforms, rendered from the platform table a
line per machine, `<platform> installs|refuses <checks> <system>
<machine>`, the checks `none` where it refuses. So
the script installs the release it is attached to, never the latest: a
pre-release is never GitHub's latest. The source names no release and no
platform; run as it is, it is refused.

It picks the tarball of the machine's platform, downloads it and
`SHA256SUMS` from the same release with `curl` or `wget`, whichever is
there, and refuses a tarball whose SHA-256, by `sha256sum` or `shasum -a
256`, differs from the sums or which the sums do not name. It unpacks the
tarball whole into `$XDG_DATA_HOME/lela/<version>`,
`~/.local/share/lela/<version>` where that is unset, since the XDG Base
Directory specification gives `~/.local/share` to an application's own
files, and links `~/.local/bin/lela` to its `bin/lela`; `lela-cgi`,
`lela-dbg` and `lela-fpm` stay in the directory's `bin/` and `sbin/`. The script reads
the line of its tarball in `SHA256SUMS` and passes over the others, which
name `install.sh` and `platforms.json` too (#32), so a user who downloads
the script before running it can check it against the sums:

```sh
curl -fsSLO https://github.com/lelaphp/lela/releases/download/v<version>/install.sh
curl -fsSLO https://github.com/lelaphp/lela/releases/download/v<version>/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS && sh install.sh
```

`shasum -a 256 -c --ignore-missing SHA256SUMS` reads it the same where
there is no `sha256sum`.

The script and the sums still come from the one release page, unsigned,
so the sum vouches for a download against the page and not for the page.

- It takes the row of `uname -s` and `uname -m`, on a Mac whose
  `sysctl -n hw.optional.arm64` says 1 the machine `arm64`, since a shell
  under Rosetta says `x86_64` there too. A row that says `refuses` is
  refused, naming the release to download its tarball from. A row that
  installs is held to the checks it names, `before_<checks>` before
  anything is fetched and `after_<checks>` over the unpacked tarball, the
  functions `dist/release.py` reads the script's checks from; the checks
  follow the row, never `uname -s`.
- `macos`: macOS on Apple silicon alone, a shell under Rosetta included:
  an Intel Mac, which no row names, is refused, and so is a macOS older
  than the `macos` line of the tarball's `bundled-libraries.txt`, both by
  name.
- `ubuntu`: Linux on `x86_64` and `aarch64`, Ubuntu of that release
  alone, read from `/etc/os-release`; each package of
  `runtime-packages.txt` that `dpkg-query` says is not installed is named,
  with the `sudo apt-get install` line that installs them, which the
  script does not run.
- Any other platform is refused, naming those the installer installs on,
  the rows of `platforms.json` that say it does.

Every check runs before anything is put in place, the unpacked `lela -v`
among them, and a refusal leaves the home as it was. It asks for no
root, refuses to run under `sudo`, writes nowhere but those two
directories, leaves any `php` and any `~/.local/bin/lela` that is not its
own link as it is, and edits no shell profile: it says when
`~/.local/bin` is not on `PATH`. A run over an installed release replaces
it in place, an upgrade removes the release it replaces, and the link
moves only once `lela -v` runs through it.

`test_install.py` runs the staged script under `sh`, and `dash` where it
is there, over a release of the test's own, its tarball packed by the
staging's `pack`, fetched from a `file://` URL: `LELA_INSTALL_FROM` names
the directory the assets are fetched from in place of the release's, and
`LELA_OS_RELEASE` the file read in place of `/etc/os-release`. Each is
named for this script alone, so no other program's setting reaches it.
`uname`, `sysctl`, `sw_vers`, `dpkg-query` and `id` are stand-ins on the
test's `PATH`. With `shellcheck` there, it holds the script to `shellcheck
-s sh`.

## The compiled tier

The hosts carry the compiled tier: `sapi/cli`, `sapi/fpm` and `sapi/makala`
each turn `php_bundle`'s `jit` feature on by default, so the release build
needs no flag for it. `opcache.jit` defaults to `disable`, as php's does, so a
request runs on the interpreter until `opcache.jit=tracing` (with
`opcache.enable_cli=1` in the CLI) asks for the tier. Tracing is experimental:
it sits outside every supported combination (ADR-0017). A host built with
`--no-default-features` carries no tier.

## Verify and refresh

`dist/inputs.toml` holds the full php-src commit, both ini SHA-256 values, the
SHA-256 of the two FPM configuration templates the `fpm` image renders, the
versioned rustup installer SHA-256 for each build architecture, and the SHA-256
of `rust-toolchain.toml`. `dist/inputs.py` compares the source identity and
toolchain file before downloading, then verifies each download before use.
A mismatch exits nonzero with the file to refresh. Docker runs that same
verifier; a cached layer is invalidated when an input or pin changes.

Run the offline tests and the checkout's pin comparison with Python 3.11+:

```sh
python3 -m unittest discover -s dist -p 'test_*.py' -v
python3 dist/inputs.py check
```

The tests exercise byte rejection, incomplete downloads, oracle commit/tag/version
drift, toolchain channel/component drift, both installer architectures, and the
FPM templates rendered as the official fpm image's `configure` renders them;
with the pinned php-src tree on the machine, found as xtask finds it by
`scripts/php_src_tree.py` (`TEST_PHP_SRCDIR`, a leading `~/` or `$HOME/`
read as the home directory, else the pin's `[source].path`), they also hold
the real templates to their pins. `test_smoke.py` holds the smoke's refusal of
arguments it does not take. `test_publish.py` holds the publish script's
dry run for a commit on `main`, a release and a pre-release, and its two
steps against a docker that records what it is asked; `test_prune.py`
holds the deletion script's dry run over the listing in `fixtures/`.
`test_release.py` holds the release script's refusal of a tag the
manifest does not name, its dry run for a release and a pre-release, and
its steps against a docker, a cargo and a gh that record what they are
asked. Both run their script in a repository of the test's own, laid
down by `testlib.py`, on a PATH of the test's own directories alone: the
recording tools, and links to git and the few programs the scripts call
that are no docker, gh or cargo. `test_bundle.py` holds the macOS
bundling on stand-in Mach-O files, answered in the real `otool -l` text
captured in `fixtures/otool/`. No test builds or runs a host; on an Apple
silicon Mac the darwin tests link programs and libraries of a few lines
with `cc`, in the forms Homebrew's kegs take, bundle them with the real
`otool`, `install_name_tool` and `codesign`, and run them with their keg
removed. `test_apt.py` holds `apt.sh`, the one way the Dockerfile,
the Linux smoke and the oracle's Dockerfile, `ci/oracle/Dockerfile`, run
apt, against an `apt-get` that fails as the test says: which steps it
tries again, how often and after what wait, and the status a mirror that
stays down leaves; that no `RUN` of either Dockerfile runs apt except
through it; and that `scripts/oracle-build.py` lays it in the oracle's
build context. `test_release.py` holds the smoke's container script to it
the same way.

To exercise real downloads without installing anything:

```sh
python3 dist/inputs.py ini /tmp/dist-inputs/ini
python3 dist/inputs.py fpm /tmp/dist-inputs/fpm
python3 dist/inputs.py rustup amd64 /tmp/dist-inputs/amd64/rustup-init
python3 dist/inputs.py rustup arm64 /tmp/dist-inputs/arm64/rustup-init
docker buildx build -f dist/Dockerfile --target ini .
```

When a person moves the oracle, resolve its source tag to the full commit in a
trusted php-src checkout (`git rev-parse '<tag>^{commit}'`). Confirm that commit
extends `oracle.toml`'s abbreviated commit and that the tag matches the oracle
version. Hash each ini file and each FPM template at that commit with SHA-256
and review the bytes. Update the tag, full commit, ini and FPM hashes together
in `dist/inputs.toml`.
The verifier compares all three source/version fields, so a tag-only move
also refuses stale pins. It does not contact a mutable remote tag at build time;
the reviewed tag-to-commit relationship is recorded in the pin file.

After a reviewed toolchain change, update its file SHA-256 in `dist/inputs.toml`.
Docker installs the channel and components from that exact toolchain file.
For a rustup update, use the versioned
`https://static.rust-lang.org/rustup/archive/<version>/<host>/rustup-init.sha256`
for both hosts listed in the pin file, then verify the downloaded installers
against those published hashes. Do not derive a new trusted hash solely from
the download being installed.

The Dockerfile frontend and Ubuntu base use multi-platform image index digests
in `dist/Dockerfile`. Inspect replacement indices for both linux/amd64 and
linux/arm64 before updating them. An intentional build-argument override of
UBUNTU supplies a different base and is the caller's responsibility.

## Verification bounds

The direct curl downloads (installers and ini files) must match repository-held
SHA-256 values before use. HTTPS is required across redirects. Docker verifies
the frontend and base image content through their index digests.

Apt verifies signed repository metadata and package SHA-256 values. Package
versions and repository snapshots are not frozen here. Each apt step, of
the Dockerfile and of a Linux tarball's smoke, runs through
`dist/apt.sh`, which tries a failed `update` or package download
again a bounded number of times, since a mirror's passing 502 with no body
is a failure apt's own `Acquire::Retries` does not retry; a mirror that
stays down still fails the build with apt's message. Rustup verifies component
archives against the selected release manifest's hashes; that manifest is
retrieved over HTTPS and is not independently pinned in this repository.
Cargo uses `--locked` and the lockfile's registry checksums. These checks do
not make the whole build hermetic or guarantee byte-identical binaries.

Successful input tests do not supply the release's binary smoke evidence.
Both `binaries` targets must still be built and their `bin/lela -v` run with
the matching runtime libraries on the release runners. An image's validation
also requires building its target and running the smoke's image mode over
it.
