# Extensions

One crate per extension under `ext/` (ADR-0004), laid down by `xtask new-ext <name>`. The template's rules:

- One dependency, `php_ext`; the engine is out of reach (`[[edge]]` `ext/* → php_ext only`).
- One entry, `extension()`; anything `pub` beyond it is printed by `api-check` and is a row of the design's list of surfaces (§4.10): what the php header a dependent extension includes declares `PHPAPI`, item by item, and nothing more.
- State is on the value: request-scoped or immutable; no lock or cell (ADR-0005, `ext.no-locks`). Cross-request caching is `api::Cache`.
- Tests go through `Capture`; every native carries its witness in rustdoc, with `exit=` written by `xtask witness --accept` only.
- The phpt subset is `php-src/ext/<name>/tests`, run with `xtask phpt --subset`.

<!-- computed tree src=. depth=1 dirs name=extensions | do not edit; run computed -->
```
.
├── ctype
├── curl
├── date
├── dom
├── filter
├── hash
├── iconv
├── json
├── mbstring
├── openssl
├── pcre
├── pdo
├── pdo_mysql
├── pdo_sqlite
├── random
├── reflection
├── session
├── spl
├── standard
└── tokenizer
```
<!-- /computed in=dec63b5cf8b5af46d5c5a3d019f16e2fc18aaf8533631ba31a7234efb5c9c70b out=95ab3077f071595e43a9de51dd8b93a9260e65c36f0df5ff1ef75ce841e32fec -->

<!-- computed exec cmd="cargo xtask status --section bundle" inputs=../seams.toml name=bundle-order | do not edit; run computed -->

| crate | order | extensions | reading | message |
|---|---|---|---|---|
| php_bundle | topological | 20 | pass | 21 workspace dependencies within its edges |

<!-- /computed in=3afc39d7a8cc10fa2bd6f9e185be3edc5ffafdc3acfdb5bfa465b78ed09c3faa out=5af62c60dedefc0f13b2aed2e1c238fb143ddc0618a5700ef1c45b328bcc4a55 -->
