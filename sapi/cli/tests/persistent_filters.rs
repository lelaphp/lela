//! A persistent stream's filters end with the stream at the engine's end,
//! as php's CLI frees `EG(persistent_list)` at `php_module_shutdown` (#2861):
//! what the closing flush raises there is labelled `PHP Shutdown`, after
//! what the request wrote. Held on the binary, since the engine's end is
//! the host's.

use std::process::{Command, Stdio};

#[test]
fn a_persistent_streams_filter_ends_at_the_engines_end() {
    let program = r"
$s = stream_socket_server('tcp://127.0.0.1:0');
$a = pfsockopen(...explode(':', stream_socket_get_name($s, false)));
stream_filter_append($a, 'convert.base64-encode', STREAM_FILTER_WRITE);
stream_filter_append($a, 'bzip2.decompress', STREAM_FILTER_WRITE);
fwrite($a, 'x');
echo 'end', PHP_EOL;
";
    let out = Command::new(env!("CARGO_BIN_EXE_php"))
        .args(["-n", "-r", program])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("running php: {e}"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "end\n\nNotice: PHP Shutdown: bzip2 decompression failed in Unknown on line 0\n"
    );
}
