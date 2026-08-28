# Contributing to MarkRust

Thank you for your interest in MarkRust!

## Development setup

- **Rust 1.80+** (MSRV; see `rust-toolchain.toml`)
- macOS or Linux recommended for GPUI development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Pull requests

1. Fork and create a feature branch from `main`.
2. Keep changes focused; add tests for core behavior.
3. Ensure CI passes (`fmt`, `clippy -D warnings`, `test`).
4. Update `CHANGELOG.md` for user-visible changes.

## Architecture docs

- [docs/architecture.md](docs/architecture.md)
- [docs/delimiter-masking.md](docs/delimiter-masking.md)

## Licensing

All contributions are licensed under MPL-2.0. Include the standard MPL file header in new Rust source files.
