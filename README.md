# cerebrai-crates

Cargo workspace of Cerebrai library crates.

## Crates

| Crate | What it does | Used by |
| --- | --- | --- |
| [`cerebrai-python`](crates/cerebrai-python) | Downloads, verifies and unpacks [python-build-standalone](https://github.com/astral-sh/python-build-standalone) distributions | `brainctl`, `workctl` |

The crates are not on crates.io. Depend on them from git:

```toml
[dependencies]
cerebrai-python = { git = "https://github.com/cerebrai-app/cerebrai-crates", tag = "v0.1.0" }
```

`tag` keeps a build reproducible; `Cargo.lock` records the exact commit either way. Run
`cargo update -p cerebrai-python` to pick up a new tag after changing it.

## Layout

- `crates/*` – library crates (each inherits version, edition, and lints from the root `Cargo.toml`)
- `xtask/` – repo automation, not published

## Development

```bash
cargo xtask fmt    # format
cargo xtask lint   # fmt check + clippy
cargo xtask test   # tests
cargo xtask ci     # lint + test
```

Add a new crate by creating `crates/<name>/`; the `crates/*` glob picks it up automatically.
