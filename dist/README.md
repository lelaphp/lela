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
| `cli` | the CLI and CGI hosts | `php -a` | `/` |
| `fpm` | those and the FPM, listening on port 9000 | `php-fpm` | `/var/www/html` |
| `apache` | those, and Apache's prefork MPM in front of the FPM over a unix socket | `apache2-foreground` | `/var/www/html` |

```sh
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target cli -t lela:cli --load .
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target fpm -t lela:fpm --load .
docker buildx build -f dist/Dockerfile --platform linux/arm64 --target apache -t lela:apache --load .
```

All three are built on one `base` stage from one `build` stage: the hosts
compile once per platform, and the second and third target find that
compile cached. They share the Ubuntu packages the build stage reads from
the hosts' ELF files, and the compat directory, `/opt/lela/compat`, which
no image puts on `PATH`. Once the hosts carry Lela's names (ADR-0016) it
holds `php`, `php-cgi` and `php-fpm` as symlinks to them, and one line in a
Dockerfile built on the image opts in:

```dockerfile
ENV PATH=/opt/lela/compat:$PATH
```

Until then the hosts are named `php`, `php-cgi` and `php-fpm` already, and
the directory is laid down empty.

The `fpm` image's `php-fpm.conf` and `php-fpm.d/www.conf` start from
php-src's templates at the pinned commit, rendered as the official image's
`configure` renders them (`dist/inputs.py fpm`), and its `docker.conf`,
`zz-docker.conf` and `docker-fpm.ini` are the template's.

### The smoke over an image

`dist/smoke.sh --image <image> <variant>` runs the tarball's checks inside
the image, on the image's own platform, over the hosts in `/usr/local`
(`php-fpm -t` where the variant holds the FPM). It checks the ini
directory, and that the compat directory is there and off `PATH`, and
that each entry of it is what its name finds once the directory is first
on `PATH`, and runs: that makes putting it first the only step the usual
names need. Once the hosts carry Lela's names, each usual name the
variant holds must be an entry; until then the directory is empty, and
passes. Then it checks what a user
of the variant sees: `cli` passes an option to `php` through the
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
  BuildKit's SBOM attached. A failed smoke pushes nothing. Each push is
  an image index, whose metadata names no config, so the pushed image's
  config is read from the registry, from the index's manifest for the
  platform and never its attestation manifest, and is refused unless it
  is the config of the image smoked. A real build
  refuses a platform the daemon does not run as its own, as
  `dist/release.py build` does, so no image smoked under emulation is
  pushed; the smoke alone still runs one. It writes one
  `<platform> <variant> <digest>` line per variant.
- `join` reads both platforms' lines and puts each variant's two images
  under one manifest list, carrying every tag of the variant:

  | ref | tags |
  |---|---|
  | `main` | `edge-<variant>`, and `sha-<commit>-<variant>` with the full commit |
  | `v<version>` | `<version>-<variant>`, and the bare `<variant>` unless the version is a pre-release or older than a release tag the checkout holds |

  Whether a release is the newest is read from the checkout's release
  tags, so a shallow checkout, which may hold its own tag alone and would
  move the bare variant for every backport, is refused, saying to fetch
  the whole history and every tag (`git fetch --unshallow --tags`,
  actions/checkout's `fetch-depth: 0`). A real `join` refuses a tree with
  changes and a release whose tag is not HEAD, as a real `build` does.
  It writes one `<variant> <digest>` line per list, the digest read from
  the descriptor in the metadata `imagetools create` writes, which names
  no `containerimage.digest`; a descriptor with no digest is refused.

Each image is labelled, and each list annotated, with standard OCI keys
alone: `source`, `revision`, `version` (`edge` from `main`) and
`licenses`, read from the workspace manifest's package table, and
`description`, `Lela`, which replaces the Ubuntu base's. A release's
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
unless a kept manifest list names it or it is a referrer of a kept one. A
version tag, a bare variant, an `edge-` tag and a tag of any other kind
are never deleted. `dist/prune.py --dry-run` names each version it would
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
  `dist/smoke.sh` runs `-v`, `-r`, a file with arguments, code on stdin,
  `php-cgi` on a file and `php-fpm -t`, a Linux tarball in the Ubuntu image
  the Dockerfile builds on with its `runtime-packages.txt` installed. The
  macOS hosts keep the linker's ad-hoc signature, which `build` reads
  before the smoke, with no Developer ID and no notarization.
- `publish` takes the three tarballs from the directory, refusing a
  missing one or one of another version, writes `SHA256SUMS` over them
  with `scripts/dist-sums.sh`, and creates the GitHub Release of the tag
  with the four attached. The release is marked a pre-release when the
  version is one, and the latest when its images move the bare variant
  tags, which it reads as `join` does, refusing a shallow checkout.

A real `build` or `publish` refuses a tree with changes, and a tag that is
not HEAD. `--dry-run` prints what the step would make and makes nothing:
`build` the platform, the commit, the tarball and its smoke; `publish` the
artifacts, the release's fields and the image tags `dist/publish.py` would
push, which a pre-release's are without a bare variant:

```sh
dist/release.py publish v0.1.0-alpha.1 --dry-run
```

The sums read back in the directory they are written in:

```sh
(cd /tmp/release && shasum -a 256 -c SHA256SUMS)
```

Docker Desktop's default VM (about 4 GiB) links one host at a time, so
`CARGO_BUILD_JOBS` stays at its default of one. NAME (default `php`) is the
hosts' name (ADR-0016).

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
with the pinned php-src tree at `TEST_PHP_SRCDIR`, they also hold the real
templates to their pins. `test_smoke.py` holds the smoke's refusal of
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
that are no docker, gh or cargo.
They do not build or run platform binaries.

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
versions and repository snapshots are not frozen here. Rustup verifies component
archives against the selected release manifest's hashes; that manifest is
retrieved over HTTPS and is not independently pinned in this repository.
Cargo uses `--locked` and the lockfile's registry checksums. These checks do
not make the whole build hermetic or guarantee byte-identical binaries.

Successful input tests do not supply the release's binary smoke evidence.
Both `binaries` targets must still be built and their `bin/php -v` run with
the matching runtime libraries on the release runners. An image's validation
also requires building its target and running the smoke's image mode over
it.
