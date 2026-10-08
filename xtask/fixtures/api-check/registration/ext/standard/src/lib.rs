//! Declares random Required, through a function returning the rows.
pub fn extension() -> Extension {
    php_ext_random::extension();
    ExtensionBuilder::default()
        .dependencies(dependencies())
        .build("standard")
}

fn dependencies() -> Vec<Dependency> {
    vec![
        Dependency {
            name: String::from("random"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        },
        Dependency {
            name: String::from("session"),
            kind: DependencyKind::Optional,
            rel: None,
            version: None,
        },
    ]
}
