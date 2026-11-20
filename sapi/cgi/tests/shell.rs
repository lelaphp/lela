//! php-cgi started from a shell names `dl()` in its `additional_functions`,
//! and under a server it names none (`Z/sapi/cgi/cgi_main.c:1874`); the
//! witness lane serves a fence only as a server would, so the shell's half
//! is held here.

fn served(env: &[(&str, &str)]) -> String {
    let dir = std::env::temp_dir().join(format!(
        "sapi-cgi-shell-{}-{}",
        std::process::id(),
        env.len()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t.php");
    std::fs::write(
        &file,
        "<?php var_dump(function_exists('dl'), (new ReflectionFunction('dl'))->getExtensionName());\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lela-cgi"))
        .env_clear()
        .envs(env.iter().copied())
        .args(["-n", "-q"])
        .arg(&file)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn dl_is_declared_from_a_shell() {
    assert_eq!(served(&[]), "bool(true)\nstring(8) \"standard\"\n");
}
