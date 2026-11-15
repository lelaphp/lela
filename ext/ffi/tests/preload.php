<?php
// The file oracle.toml's `[regime.ffi-preload]` hands `opcache.preload`:
// under `ffi.enable=preload` each function declared here may use FFI in every
// request after it, in any host, as php's ZEND_ACC_PRELOADED caller may.
function preloaded_abs(int $n): int
{
    return FFI::cdef("int abs(int j);")->abs($n);
}

class PreloadedFfi
{
    public static function abs(int $n): int
    {
        return FFI::cdef("int abs(int j);")->abs($n);
    }
}
