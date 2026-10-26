#!/bin/zsh
# capture.sh [php]: the oracle's SSA dumps of each program of
# ../types/goldens, written to goldens/<program>.ssa, and of the seven
# kernels, written to goldens/kernels/<kernel>.ssa, which
# analysis/tests.rs holds the tier's dumps to.
#
# The settings are the oracle's recipe and its `[regime.jit-ssa]`, both
# read from `oracle.toml`: php's command-line tier on, tracing, every
# counter hot at 1, the optimizer off, so php analyses the op arrays the
# compiler made as the engine does, and `opcache.jit_debug=258050`, the SSA
# dump (1<<1) with the TRACE lines. Each op array's dump is kept once, in
# the order php first wrote it, its file named by its base name. `php`
# defaults to the oracle's binary, `oracle.toml`'s `path`.
set -eu
here=${0:A:h}
oracle=${here:h:h:h:h}/oracle.toml
php=${1:-$(sed -n 's/^path = "\(.*\/bin\/php\)"$/\1/p' $oracle | head -1)}
# The quoted entries of the recipe's `ini` array, then of the regime's.
ini=(${(f)"$(awk '
  /^\[/ { section = $0 }
  section == "[recipe]" && /^ini = \[/ { list = 1; next }
  list && /^\]/ { list = 0 }
  list { gsub(/^ *"|",? *$/, ""); print }
  section == "[regime.jit-ssa]" && /^ini = \[/ {
    sub(/^ini = \["/, ""); sub(/"\]$/, ""); n = split($0, entries, /", "/)
    for (i = 1; i <= n; i++) print entries[i]
  }' $oracle)"})
args=(-n)
for i in $ini; do args+=(-d $i); done
# capture <dir> <program> <out>: php's dumps of <dir>/<program> into <out>.
capture() {
  ( cd $1 && env -i TZ=UTC LANG=C LC_ALL=C PHPRC=/dev/null HOME=/tmp $php $args $2 2>&1 >/dev/null ) \
    | awk -v file="${1:A}/$2" -v name="$2" '
      # A dump starts at its name, the line before its "(lines=" line, and
      # ends at the next dump or the next TRACE line.
      function flush() {
        if (n == 0) return
        while (n > 0 && body[n] == "") n--
        text = ""
        for (i = 1; i <= n; i++) text = text body[i] "\n"
        if (!(text in seen)) { seen[text] = 1; printf "%s\n", text }
        n = 0
      }
      /^---- |^     TRACE |^JIT/ { flush(); prev = ""; next }
      /^     ; \(lines=/ && prev ~ /:$/ {
        if (n > 0) n--
        flush(); body[++n] = prev
      }
      {
        line = $0
        at = index(line, file)
        if (at > 0) line = substr(line, 1, at - 1) name substr(line, at + length(file))
        if (n > 0 || line ~ /^     ; \(lines=/) body[++n] = line
        prev = $0
      }
      END { flush() }' > $3
}
for program in $here/../types/goldens/*.php; do
  capture ${program:h} ${program:t} $here/goldens/${program:t:r}.ssa
done
# The seven kernels of docs/perf/workloads, each `@bench-param` set small,
# as analysis/tests.rs sets it (KERNELS): the analysis is of the op arrays,
# which the size does not change, and a debug engine serves them in seconds.
kernels=(fib 10 nbody 100 spectralnorm 10 mandelbrot 20 binary_trees 6 fannkuch 6 string_munge 200)
small=$(mktemp -d)
mkdir -p $here/goldens/kernels
for name n in $kernels; do
  sed -E "s/^\\\$N = [0-9]+; \/\/ @bench-param/\\\$N = $n; \/\/ @bench-param/" \
    ${here:h:h:h:h}/docs/perf/workloads/$name.php > $small/$name.php
  capture $small $name.php $here/goldens/kernels/$name.ssa
done
rm -rf $small
