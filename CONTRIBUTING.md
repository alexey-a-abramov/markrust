# Contributing to MarkRust

Thank you for your interest in MarkRust!

## Development setup

- **Rust 1.96** (pinned in `rust-toolchain.toml`)
- macOS or Linux recommended for GPUI development

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo bench -p markrust-core --bench parse
cargo bench -p markrust-editor --bench layout
```

The website is part of the release surface. From the repository root, install
its dependencies and run its unit and browser tests before changing public
documentation:

```bash
pnpm install --frozen-lockfile
pnpm --dir website exec playwright install chromium # first time only
pnpm --dir website test
```

## Manual GUI validation

Headless tests intentionally do not open a native GPUI window. For changes to
the editor surface, perform a short GUI smoke test on the platform you changed.
Before a macOS release, also verify a CJK IME candidate window in a body
paragraph, wrapped line, table cell, code-language chip, image caption, and
frontmatter field. Record the OS, IME, build, and result with the release.

## Pull requests

1. Fork and create a feature branch from `main`.
2. Keep changes focused; add tests for core behavior.
3. Ensure CI passes (`fmt`, `clippy -D warnings`, `test`).
4. Update `CHANGELOG.md` for user-visible changes.

## Architecture docs

- [Engineering documentation index](docs/README.md)
- [Architecture](docs/architecture.md)
- [Delimiter masking](docs/delimiter-masking.md)
- [WYSIWYG engineering notes](docs/roadmap.md)

## Licensing

All contributions are licensed under MPL-2.0. Include the standard MPL file header in new Rust source files.
