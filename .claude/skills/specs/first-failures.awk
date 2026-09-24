# The first failure of each failing test in `xtask phpt` output, one cause per
# test: the first line the engine printed that php didn't, or, where the engine
# printed nothing php didn't, the first line php printed that the engine didn't
# (`php only:`, where a compile or link fatal php raises lands). Named by what
# is missing where it can be, masked of strings, paths and numbers where not.
#   awk -f first-failures.awk <run> | sort | uniq -c | sort -rn | head -40
function mask(s) {
    gsub(/"[^"]*"/, "_", s); gsub(/[^ ]+\.(php|inc)/, "_", s); gsub(/[0-9]+/, "_", s)
    return substr(s, 1, 100)
}
function cause(s) {
    if (match(s, /Call to undefined function [A-Za-z0-9_\\]+/))  return "undefined function " substr(s, RSTART + 27, RLENGTH - 27)
    if (match(s, /Call to undefined method [A-Za-z0-9_\\:]+/))   return "undefined method " substr(s, RSTART + 25, RLENGTH - 25)
    if (match(s, /(Class|Interface|Trait|Enum) "[^"]+" not found/)) return substr(s, RSTART, RLENGTH)
    if (match(s, /Undefined constant "[^"]+"/))                  return "undefined constant " substr(s, RSTART + 20, RLENGTH - 21)
    if (s ~ /(Fatal error|Warning|Deprecated|Notice|Parse error)/) return mask(s)
    return "output differs"
}
function close_test() {
    if (cur && !seen) print "php only: " (minus == "" ? "(nothing)" : mask(minus))
    cur = 0
}
/^[a-z]+ phpt:/ { close_test(); cur = ($1 == "fail"); seen = 0; minus = ""; next }
/^[a-z]/        { close_test(); next }
cur && !seen && /^\+./ { seen = 1; print cause(substr($0, 2)); next }
cur && minus == "" && /^-[^ ]/ && /(error|Warning|Deprecated|Notice)/ { minus = substr($0, 2) }
END { close_test() }
