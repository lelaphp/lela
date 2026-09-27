Neighbours judged in one batch, as run-tests.php hands a worker its batch and
the worker runs it one test after another (run-tests.php:1537-1546,
1742-1744). `a-temp.phpt` and `b-temp.phpt` each make and remove
`relative_temp` relative to the root every program runs from, as ext/zlib's
readgzfile_basic.phpt and readgzfile_basic2.phpt make `readgzfile_temp`
(#1091); run at once, one of them fails on `mkdir(): File exists`. Three tests
on a pool of eight make batches of two, so the pair shares one and
`c-free.phpt` runs beside it in its own, the negative control that shows the
pool runs anything at once at all. Each program writes the span it ran over
beside itself. Held by `xtask/src/phpt/tests.rs`.
