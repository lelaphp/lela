//! Declares standard Required through a helper function and apc Optional
//! through a closure; links random, which it declares nowhere.
pub fn extension() -> Extension {
    php_ext_random::extension();
    php_ext_standard::extension();
    let dep = |name: &str, kind: DependencyKind| Dependency {
        name: name.into(),
        kind,
        rel: None,
        version: None,
    };
    ExtensionBuilder::default()
        .dependencies(vec![required("standard"), dep("apc", DependencyKind::Optional)])
        .build("session")
}

fn required(name: &str) -> Dependency {
    Dependency {
        name: String::from(name),
        kind: DependencyKind::Required,
        rel: None,
        version: None,
    }
}
