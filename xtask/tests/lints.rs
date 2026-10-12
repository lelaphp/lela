//! The workspace's lints refuse an owner dropped in silence (#1980,
//! #1998): a member crate inherits the root manifest's
//! `[workspace.lints]`, and clippy with every warning denied, as the
//! `clippy` instrument runs it, refuses `let _ = owner;` and
//! `mem::forget(owner)` and passes an owner given up.
//!
//! The forms clippy has no lint for are held at run time instead, by a
//! debug build's `Owner` drop that names the line an owner was made on:
//! `_ = owner;`, `drop(owner)`, `let _owner = owner;`, and an owner
//! wrapped in a type that is not must-use (`let _ = Some(owner);`).

use std::path::Path;
use std::process::Command;

use xtask::scratch::Tree;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The shape of `php_engine::api::values::Owner` in a debug build:
/// must-use, neither `Copy` nor `Clone`, with a `Drop` that sees an
/// owner dropped unconsumed. The engine's own type is not built here,
/// since what is held is the workspace's lints, not the engine.
const OWNER: &str = r#"
#[must_use = "an owner is handed to a sink or given up with Owner::give_up"]
pub struct Owner(u32);

impl Owner {
    pub fn take(claims: u32) -> Owner {
        Owner(claims)
    }

    pub fn give_up(mut self) {
        self.0 = 0;
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        assert_eq!(self.0, 0, "an owner dropped unconsumed");
    }
}
"#;

/// The root manifest's `[workspace.lints]`, the table every member's
/// `[lints] workspace = true` inherits.
fn workspace_lints() -> toml::Value {
    let text = std::fs::read_to_string(workspace().join("Cargo.toml")).unwrap();
    let manifest: toml::Table = toml::from_str(&text).unwrap();
    manifest["workspace"]["lints"].clone()
}

/// Clippy over one crate laid down in `root` under the workspace's
/// lints, its library the stand-in owner and `body`: whether it passed,
/// and what it printed.
fn clippy(root: &Tree, name: &str, body: &str) -> (bool, String) {
    let dir = root.join(name);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let mut manifest = toml::Table::new();
    manifest.insert(
        "package".into(),
        toml::toml! { name = "silent_drop" version = "0.0.0" edition = "2024" }.into(),
    );
    let mut table = toml::Table::new();
    table.insert("lints".into(), workspace_lints());
    manifest.insert("workspace".into(), table.into());
    manifest.insert("lints".into(), toml::toml! { workspace = true }.into());
    std::fs::write(dir.join("Cargo.toml"), toml::to_string(&manifest).unwrap()).unwrap();
    std::fs::write(dir.join("src/lib.rs"), format!("{OWNER}\n{body}")).unwrap();
    let out = Command::new("cargo")
        .args(["clippy", "-q", "--", "-D", "warnings"])
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn clippy_refuses_an_owner_discarded_by_let_underscore_or_forgotten() {
    let root = Tree::new("lints-silent-drop").unwrap();
    std::fs::copy(
        workspace().join("rust-toolchain.toml"),
        root.join("rust-toolchain.toml"),
    )
    .unwrap();
    for (name, body, refusal) in [
        (
            "let_underscore",
            "pub fn discarded(claims: u32) {\n    let owner = Owner::take(claims);\n    let _ = owner;\n}\n",
            Some("#let_underscore_must_use"),
        ),
        (
            "let_underscore_typed",
            "pub fn discarded(claims: u32) {\n    let owner = Owner::take(claims);\n    let _: Owner = owner;\n}\n",
            Some("#let_underscore_must_use"),
        ),
        (
            "forget",
            "pub fn forgotten(claims: u32) {\n    std::mem::forget(Owner::take(claims));\n}\n",
            Some("#mem_forget"),
        ),
        (
            "given_up",
            "pub fn given_up(claims: u32) {\n    Owner::take(claims).give_up();\n}\n",
            None,
        ),
    ] {
        let (passed, stderr) = clippy(&root, name, body);
        match refusal {
            Some(lint) => {
                assert!(!passed, "{name}: {stderr}");
                assert!(stderr.contains(lint), "{name}: refused by {lint}: {stderr}");
            }
            None => assert!(passed, "{name}: {stderr}"),
        }
    }
}
