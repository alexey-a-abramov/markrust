# MarkRust roadmap

_Last reviewed: 2026-10-04._

This is the canonical product and execution roadmap. The detailed
[WYSIWYG engineering notes](docs/roadmap.md) retain design decisions and test
evidence; they are not a competing priority list.

## Current v0.8 alpha release gate

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
- [ ] **Distribution readiness:** the four-native-runner Actions matrix and
  tagged `v0.8.1` archive publication passed. Complete Windows recovery
  ACL/locking and desktop acceptance, choose signing/notarization, publish
  checksum-backed Homebrew formulae, and make an explicit crates.io decision
  for each publishable crate.
- [ ] **Website delivery:** enable the GitHub Pages workflow, verify ownership
  of `markrust.org`, configure the documented DNS records, and enforce HTTPS.
- [ ] **Release quality:** run formatting, clippy, Rust tests, website tests,
  native GUI geometry/input regressions, reviewed screenshot comparisons,
  a release binary `--version` smoke test, and a short native GUI smoke pass.
  Native builds and screenshot capture require full Xcode and its Metal
  toolchain. See [GUI testing](docs/gui-testing.md).
- [ ] **Real update acceptance:** the `0.8.0` local seed is installed and
  `v0.8.1` is published after native/remote gates. Unlock the Mac and complete
  the in-app update; observe the new process, build identity and recovered
  unsaved draft. Checksums and helper receipts alone are not proof
  of successful startup. In-app installation currently supports macOS only;
  publisher signing/notarization remains a separate decision.

## v0.8 guarded release updates

- Background stable-release checks against the fixed GitHub repository, with a
  persisted opt-out and explicit download/restart actions.
- Bounded downloads and safe archive extraction, architecture/version/identity
  validation, checksum verification and strict ad-hoc signature checks.
- All-window private checkpoint gate, active dialog/IME protection, trusted
  helper handoff and same-volume atomic replacement with retained rollback.
- Native notice geometry/input checks and isolated corrupt-archive, race,
  cancelled-handoff and recovery-lease regressions.

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
- Per-action UI observations connect actual focus, selection direction,
  document/layout revisions, nested context and source-backed glyph geometry.
  Offline evidence timelines and optional frame capture support visual review.
- Source multi-row selection and code-highlight layering; canonical
  frontmatter focus transitions and remembered Split input pane per tab.
- Shared-document caret repair after cross-pane changes, including reversed
  and Unicode selections. WYSIWYG markup hints can be disabled without changing
  the Markdown; deep editing retains its viewport anchor.
- Stable WYSIWYG projection: context hints are paint-only tint and status text,
  not syntax inserted into shaped lines. Empty list/paragraph endpoints retain
  a painted caret; contextual table controls stay outside document flow.
- Out-of-flow link destination editing with its own visible caret, draft undo,
  explicit commit/cancel, and no document reflow when hints are disabled.
- Scrollable editor-column document tabs, explicit active/dirty state, native
  new-tab/cycling shortcuts, and basename recent files with full-path tooltips.
- Native, Ocean and Forest color-only highlight profiles for both surfaces.
- Private bounded local session recovery for open and unsaved buffers, with
  explicit warnings and publication protection when disk changed during downtime.
- Source-preserving concurrent-edit merge with bounded same-line refinement,
  checked explicit-save publication, and recoverable side-by-side resolution.
  Normalization is a separate undoable review, never a Save interruption.
- Everyday notepad lifecycle: last-active-window Finder routing, explicit New
  Window, independent private recovery sessions and graceful draft-preserving
  close/quit. Background autosave never publishes the source file. Raw widget
  and body IME drafts survive restart, with safe scratch fallback for stale
  targets. Verified locally and reinstalled on 2026-10-01; see the
  [implementation record](docs/notepad-experience.md) for evidence and limits.

## v0.4 input and projection corrections

- Literal monospace Source in both standalone and Split, with color-only
  Markdown highlighting and real mouse handler registration.
- Rich selection uses fresh painted row/caret stops, including the final glyph,
  rather than letting unrelated leaves overwrite a drag endpoint.
- Context-only floating table controls and optional passive syntax badges;
  neither participates in document layout or moves words.
- Command palette owns native query input, resolves duplicate tab titles by
  identity and restores the remembered pane on dismissal.
- Native pointer journeys join actual user events, full model table dimensions,
  input ownership and painted geometry; isolated journey filtering speeds
  diagnosis without replacing the complete GUI gate.

Next UX acceptance work remains explicit: continuous edge autoscroll while
holding a drag still and rich wordwise double-click dragging. These are not
established by short-word pointer tests. OS IME and accessibility acceptance
remain manual release gates.

## v0.5 viewport and gesture corrections

- Preserve pre-remeasurement visibility when editing a virtualized rich block;
  a temporary missing bound must not scroll an already visible block to the top.
- Leave fully visible carets in place, including near viewport edges. Reveal
  only a genuinely out-of-view caret, using a bounded caret-follow adjustment.
- Define selection by the handled press anchor and actual release position,
  preserving reverse direction and intermediate drag highlights. Release
  cannot depend on receiving a final MouseMove.
- Resolve rich paragraph padding from fresh painted rows; exclude actual
  widget, link-panel and table-control bounds, and retain control navigation.
- Floating Files/Outline panels own their presses instead of starting an
  underlying document drag. Empty space below Source text places the caret at EOF.
- Inspect the first three native paints after each action, not only eventual
  settled state. Long-document fixtures check deletion after manual scrolling
  in both editors and both Split input owners.

## v0.6 linked context and document affordances

- Passive Split-view cursor and selection project the actual input owner into
  the peer's native glyph layout. They never own input, move the real peer
  selection, request scrolling, or create history. Non-body focus suppresses
  the projection; offscreen context stays offscreen.
- Literal H1/H2/H3/Body toolbar labels, verified with real button clicks.
- Deliberate list triggering and Backspace cancellation, stable empty paragraph
  geometry after list exit, and atomic Undo for resumed character-by-character
  typing. Repeated Enter preserves the empty draft.
- Relative images and an out-of-flow image inspector with native fields,
  nearby files, file selection, safe preview, explicit Apply/Cancel, and private
  draft recovery. Split input and field Undo/Paste follow actual focus.
- Per-action native evidence joins document bytes, input ownership, context,
  caret/selection geometry and first paints. Screenshots complement these
  contracts rather than replacing them. See the
  [implementation record](docs/notepad-experience.md#version-060-linked-context-and-editing-affordances).

This is local development, not a published or installed release. Distribution,
OS IME/VoiceOver acceptance and installed-app replacement remain separate gates.
The 2026-10-04 local verification passed 1,447 Rust tests, strict Clippy,
73 native GUI states, 242 action journeys and all 32 reviewed screenshot
comparisons. Inspector-specific checks also prove actual preview pixels and
wheel isolation; see the implementation record above for evidence.

## v0.7 everyday navigation and localization

- Current-document Find with Cmd/Ctrl-F, next/previous shortcuts, match count,
  passive mapped highlights and pane-scoped navigation. Search never rewrites
  Markdown or creates history; Escape returns input to the originating pane.
- Open Location accepts a typed file/folder path without replacing dirty tabs.
  Headers show the current file path; the sidebar identifies its folder and
  separates files outside it with parent-directory context.
- Twenty embedded UI catalogs, changed immediately through View → Language
  across editor windows. Locale updates are chrome-only: no document mutation,
  caret reveal, content remeasurement or viewport reset. Initial translations,
  full RTL layout, native OS-menu/IME and screen-reader acceptance still need review.
- Consistent vector icon family, explicit H1/H2/H3/Body labels and common
  platform shortcut aliases. Theme switching no longer claims Cmd/Ctrl-Shift-T,
  reserved for future closed-tab reopening.
- Native-runner Actions builds for macOS ARM/Intel, Linux and experimental
  Windows. Push artifacts and checked version-tag Releases are separate;
  signing and platform acceptance remain explicit gates. See
  [binary distribution](docs/deployment.md#binary-distribution).

This work is local development. No installation, push, public release or
repository-settings change is implied by these prepared workflows.
Local verification on 2026-10-04 passed 1,470 Rust tests, strict Clippy,
97 native GUI states, 262 action journeys and all 32 reviewed screenshot
comparisons. Twenty Find journeys also passed with intermediate paint frames;
packaging/workflow tests (16), website unit/E2E tests (3/16) and the release
build passed. See the [implementation evidence](docs/notepad-experience.md#local-verification-2026-10-04)
for build identity, isolated artifact paths and the remaining acceptance gates.

## Improve the quality foundation next

- Complete viewport-edge vertical navigation: scroll, paint, then resolve a
  target row instead of falling back to source-line movement when it is outside
  the virtualized viewport.
- Expand deterministic action generation with structured edit/undo/selection
  sequences. All generated journeys now check state-to-render contracts;
  prioritize deeper transition coverage rather than scenario counts alone.
- Expand command-palette discoverability and accessible result navigation.
  Native search, arrow/Enter/Escape handling, tab activation and pane-focus
  restoration now have action-to-render regression coverage.
- Extend native GUI fixtures to images, frontmatter drafts, font sizes,
  display scales, and long-document scrolling. Add AppKit/VoiceOver acceptance
  and XCTest UI journeys as accessibility semantics and packaging mature.
- Stress the [everyday notepad](docs/notepad-experience.md) interruption timing,
  storage exhaustion
  and large sessions beyond the documented checkpoint limits; retain undo
  history as a later, separately bounded improvement.
- Move document save I/O off the foreground executor with revision-aware
  completion; slow-volume writes must not stall native editing or clear newer edits.
- Give atomic images/rules and fully hidden HTML an explicit keyboard object
  selection or edge-caret indicator, without reintroducing in-place syntax
  reveal or assigning their offsets to unrelated text leaves.
- Rebase inactive pane selections through edit deltas, beyond current safe
  bounds/grapheme repair, to retain semantic position after edits before them.
- Carry explicit revision provenance or stable block identities for batched
  structural edits before paint; one engine `last_splice` is not a complete
  history of list transitions. Extend granular release-coordinate coverage
  from document-body selection to editable widget drafts.
- Turn each documented normalize exception into a named fixture with a tracked
  resolution path. Add website link, accessibility, mobile, and visual checks.

## After the release gate

Prioritize a small document editor: current-document Find/Replace, reopen
closed tabs, clear private-draft status, text zoom/focus mode, and native
printing/accessibility. Windows support and custom keybindings follow alpha
feedback. Workspace search is optional, not a reason to grow an IDE shell.

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
- [Concurrent editing](docs/concurrent-editing.md) — version preservation and save boundaries
- [Everyday notepad](docs/notepad-experience.md) — private drafts, windows and lean next features
