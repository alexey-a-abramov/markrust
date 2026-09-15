# MarkRust roadmap

_Last reviewed: 2026-09-15._

This is the canonical product and execution roadmap. The detailed
[WYSIWYG engineering notes](docs/roadmap.md) retain design decisions and test
evidence; they are not a competing priority list.

## Current v0.1 alpha release gate

- [ ] **Manual CJK IME acceptance:** confirm that the macOS Hiragana or Pinyin
  candidate window follows the caret in a body paragraph, wrapped line, table
  cell, code-language chip, image caption, frontmatter title, and YAML field.
  Record the OS, IME, build, and pass/fail evidence. Unit tests cover the
  in-app IME plumbing but cannot prove the operating-system candidate window.
- [ ] **Remote-content acceptance:** run a native UX pass for the explicit
  per-tab remote-image load control, including blocked/private addresses,
  redirects, malformed responses, and accessible failure states. Fetches now
  default off, require public HTTPS endpoints, and validate `data:`, remote,
  and local SVGs through the same strict boundary.
- [ ] **Distribution readiness:** smoke-test tagged archives, publish
  checksum-backed Homebrew formulae, and make an explicit crates.io decision
  for each publishable crate.
- [ ] **Website delivery:** enable the GitHub Pages workflow, verify ownership
  of `markrust.org`, configure the documented DNS records, and enforce HTTPS.
- [ ] **Release quality:** run formatting, clippy, Rust tests, website tests,
  native GUI geometry/input regressions, reviewed screenshot comparisons,
  a release binary `--version` smoke test, and a short native GUI smoke pass.
  Native builds and screenshot capture require full Xcode and its Metal
  toolchain. See [GUI testing](docs/gui-testing.md).

## Shipped in v0.1 alpha

- Typora-style WYSIWYG, optional Source and Split modes, rich Markdown editing,
  tables, images, frontmatter, and HTML export.
- Local-first workspace shell: files, tabs, outline, command palette,
  autosave, atomic writes, and external-edit reconciliation.
- Comrak-based source spans and `RichTree`, byte-preserving saves, performance
  gates, corpus round trips, and headless app/CLI journeys.
- Grapheme-safe WYSIWYG editing, rendered soft-wrap vertical navigation,
  frontmatter YAML validation with retained invalid drafts, and strict SVG
  preflight for local, `data:`, and explicitly loaded remote images.
- Locked CI/release workflows with deterministic buffer invariants, native
  macOS smoke coverage, checksummed release archives, and website validation.
- Native macOS menus and compact toolbar icons, with direct WYSIWYG, Source,
  and Split selection and matching menu commands and keyboard shortcuts.
  Side panels collapse or float at narrow widths to preserve writing space.
- Parent-width text measurement for wrapped paragraphs, lists, tables, and
  editable widgets; selection highlights drawn per visible text row.
- Scrollable unwrapped Source lines, with horizontal caret-follow on keyboard
  navigation and resize; atomic selection replacement and clipboard undo.
- Native GUI regression fixtures and geometry/input checks, with Metal
  screenshots and explicit baseline comparison for visual review.

## Improve the quality foundation next

- Complete viewport-edge vertical navigation: scroll, paint, then resolve a
  target row instead of falling back to source-line movement when it is outside
  the virtualized viewport.
- Add generated edit/undo/selection sequences for the rich-editor state
  machine, extending the deterministic UTF-8 rope and line-index invariants.
- Extend native GUI fixtures to images, frontmatter drafts, font sizes,
  display scales, and long-document scrolling. Add AppKit/VoiceOver acceptance
  and XCTest UI journeys as accessibility semantics and packaging mature.
- Turn each documented normalize exception into a named fixture with a tracked
  resolution path. Add website link, accessibility, mobile, and visual checks.

## After the release gate

Prioritize from alpha feedback: Windows support, accessibility, workspace
search, custom keybindings, and safe remote-content controls.

## Explicitly deferred

- Git UI integration
- Cloud sync and accounts
- Plugin marketplace
- MCP / AI agent server
- Full TeX/LaTeX and Mermaid rendering

## Related

- [WYSIWYG engineering notes](docs/roadmap.md) — detailed design and evidence
- [Architecture](docs/architecture.md) — current module and data-flow design
- [Contributing](CONTRIBUTING.md) — local validation workflow
- [Website deployment](docs/deployment.md) — Pages and DNS handoff
- [GUI testing](docs/gui-testing.md) — native rendering regressions and review
