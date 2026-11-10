# cerebrai-crates

Cargo workspace of Cerebrai library crates.

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
