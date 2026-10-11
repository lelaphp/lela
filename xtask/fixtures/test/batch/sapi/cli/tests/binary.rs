//! The binary as this host's own features build it (#2606).

#[test]
fn the_binary_is_built_with_this_host_s_features() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_php_sapi_cli"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
}
