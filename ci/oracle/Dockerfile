# The Linux oracle builds (#1782, #1889, #1891), laid by
# `scripts/oracle-build.py build linux-<arch> <variant>`, which passes every
# argument below and the context: the pinned tag as `git archive` writes it,
# the configure line it read off the pinned oracle, and CI's package list.
#
# `deps` is ubuntu:26.04 by the digest of the platform's manifest, with the
# packages CI's engine build links (.github/actions/platform-libraries) and
# the ones the oracle's configure line needs beyond them. On amd64 they come
# from Ubuntu's archive snapshot SNAPSHOT, so a rebuild installs the same
# versions; snapshot.ubuntu.com serves no ubuntu-ports (HTTP 401), so on
# arm64 they are the archive's of the day. Either way the versions installed
# are written beside the tree, and the recipe copies them out with `php`.
# `build` lays the tree at the path every platform builds at (the recipe's
# BUILD, /tmp/dax-php-oracle, spelled here as there) and installs
# `php` and `php-fpm` at the variant's prefix, where a runner takes them out
# of the image (#1948; php-fpm for the bench's baseline, #3385, #3774).
ARG BASE
FROM ${BASE} AS deps
ARG SNAPSHOT
COPY packages.txt /tmp/packages.txt
# Beyond CI's list, what the oracle's configure line needs:
#   build-essential autoconf bison re2c  buildconf, the lexers and parsers,
#                                        since a tag carries none generated
#   systemtap-sdt-dev  --enable-dtrace     libbz2-dev     --with-bz2
#   libldap-dev        --with-ldap         libsasl2-dev   --with-ldap-sasl
#   libsnmp-dev        --with-snmp         libpq-dev      --with-pgsql, --with-pdo-pgsql
#   unixodbc-dev       --with-unixODBC, --with-pdo-odbc
#   freetds-dev        --with-pdo-dblib    libargon2-dev  --with-password-argon2
#   libzip-dev         --with-zip          libgdbm-compat-dev  --with-ndbm
# --with-ndbm looks for dbm_open in -lndbm, -ldb1 or the C library
# (ext/dba/config.m4); macOS's C library has it, and Ubuntu ships gdbm's
# ndbm as libgdbm_compat, so libndbm.so is laid as a link to it, and the
# binary needs libgdbm_compat.so.4 by its soname. Homebrew's own Linux
# build passes --without-ndbm instead, which would drop a handler
# `dba_handlers()` lists.
# The snapshot is https and the base image has no CA bundle, so apt skips
# the TLS peer check for that one host; it still holds every index to the
# archive's signature, as it does over the archive's plain http.
RUN set -eu; \
    if [ "$(dpkg --print-architecture)" = amd64 ]; then \
        printf 'APT::Snapshot "%s";\nAcquire::https::snapshot.ubuntu.com::Verify-Peer "false";\n' "$SNAPSHOT" \
            > /etc/apt/apt.conf.d/50snapshot; \
    fi; \
    apt-get update -qq; \
    sed -e 's/#.*//' -e 's/[[:space:]]*$//' -e '/^$/d' /tmp/packages.txt > /tmp/packages; \
    DEBIAN_FRONTEND=noninteractive xargs -a /tmp/packages apt-get install -y -qq --no-install-recommends \
        build-essential autoconf bison re2c \
        systemtap-sdt-dev libbz2-dev libldap-dev libsasl2-dev libsnmp-dev libpq-dev \
        unixodbc-dev freetds-dev libargon2-dev libzip-dev libgdbm-compat-dev; \
    rm -rf /var/lib/apt/lists/*; \
    ln -s libgdbm_compat.so "/usr/lib/$(gcc -print-multiarch)/libndbm.so"; \
    mkdir -p /tmp/dax-php-oracle; \
    dpkg-query -W -f '${Package}=${Version}\n' > /tmp/dax-php-oracle/installed.txt

FROM deps AS build
ARG VARIANT
ARG JOBS=4
ARG SOURCE_DATE_EPOCH
WORKDIR /tmp/dax-php-oracle/${VARIANT}/php-src
COPY php-src.tar configure.args /tmp/dax-php-oracle/${VARIANT}/
# Each step under `env -i` with only these variables, as on macOS: no CFLAGS
# or locale of the image's reaches the build.
RUN set -eu; \
    tar -xf ../php-src.tar; \
    set -- env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/root TZ=UTC LC_ALL=C SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH"; \
    "$@" ./buildconf --force; \
    xargs -d '\n' -a ../configure.args "$@" ./configure; \
    "$@" make -j"$JOBS"; \
    "$@" make install-cli; \
    "$@" make install-fpm
