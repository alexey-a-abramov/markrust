# MarkRust architecture

MarkRust is a **true WYSIWYG** Markdown editor: the user edits a rendered rich document; Markdown is the on-disk serialization. Source displays literal Markdown with uniform monospace metrics. Split puts that Source editor beside an editable WYSIWYG view of the same document. All native Rust on GPUI.

The rope buffer is the single source of truth. A derived `RichTree` (comrak) is authoritative for interpretation and command targeting. Source-mode masking is a projection of that same grammar, not a second Markdown parser.

## Crate boundaries

| Crate | Responsibility | GUI deps |
|---|---|---|
| `markrust-core` | Rope buffer, undo, line index, comrak `RichTree` + source spans, revision tokens | None |
| `markrust-editor` | `HeadlessEditor` + source masking/layout; GPUI `MarkdownEditor` and `RichEditorView` | GPUI (view only) |
| `markrust-app` | `HeadlessWorkspace` session + GPUI window chrome | GPUI (Zed git pin) |
| `markrust` | CLI (`parse_args` / `run`) and desktop binary | GUI only for `--gui` |

**Rule:** `HeadlessEditor` (`markrust-editor::headless`) and `HeadlessWorkspace` (`markrust-app::session`) must not use GPUI types. Native and headless workspace policies currently have separate adapters; native journeys must test the real adapter, not assume a headless result proves it.

## UI language and auxiliary input boundary

`markrust-app::i18n` owns 20 embedded catalogs and stable ISO language choices;
`AppConfig` persists the choice and migrates older/unknown choices to English
without resetting other preferences. The desktop window registry propagates
changes to existing windows and the configuration used by future windows.
`EditorTheme` carries an immutable shared catalog, keeping editor crates
independent of the application's language enum. Locale changes update chrome
strings only, including open inspector fields: never call the content theme's
remeasure/caret-reveal path. Document bytes, Markdown, filenames, history and
real selections are not translated. Unknown diagnostic labels retain English
fallback. Catalog tests reject duplicate/empty/missing keys and altered
placeholders. Native locale journeys check focus, deep viewport and document
invariants; OS menus, full RTL layout and native-speaker review remain separate.

Find is window-owned auxiliary plain-text input over source-addressed ranges,
not a second document model. Both native editors paint passive highlights from
the same revision; only the originating pane may reveal the active result.
Open Location follows the existing owned file/folder route after canonical path
validation, keeping dirty tabs and modal input ownership intact.

## Headless command layer

```
Native: GPUI window → Workspace → MarkdownEditor / RichEditorView
Tests: WorkspaceCommand → HeadlessWorkspace → HeadlessEditor
        ↓  shared Document / RichEngine + editor commands
Document + caret/selection → RichTree
        ↓
Source: compute_visibility / build_display_layout
Split source: identity byte projection / syntax styling
WYSIWYG: RichEditorView over RichTree
export HTML (comrak, same extension set as import)
```

- `EditorCommand`: insert, backspace, delete, word/line delete, move/select caret, undo/redo, jump, wrap.
- `RichCommand`: WYSIWYG typing, marks, lists, tables, frontmatter fields — compiled to byte splices on the rope.
- `WorkspaceCommand`: save/open/export, drop files, theme, tabs, heading jump, `AdvanceTime` (fake clock), external-change reload, `SaveWithReview`.
- Drop classification stays pure in `drop.rs`.
- File-watcher policy is `classify_external_change` (ignore own saves; three-way merge dirty tabs; review conflicts). The native workspace accepts clean-tab disk changes while retaining Undo.

Unit tests live next to the modules. Headless e2e lives in `crates/markrust-app/tests/e2e.rs` (no window). CLI e2e lives in `crates/markrust/tests/e2e.rs` (`assert_cmd`, never empty args / `--gui`).

## Data flow

```
Keyboard / Mouse / IME / FileWatcher
        ↓
DocumentBuffer (ropey) + UndoStack
        ↓
Revision-stamped background comrak parse
        ↓
Source syntax spans + RichEngine::sync → RichTree
        ↓
Source / Split source: literal byte layout + color-only syntax styling
WYSIWYG: virtualized blocks + native shaped text + widget overlays
```

Parse, watcher waits, explicitly approved remote-image fetches and image
decode stay off the UI thread. Images pass bounded raster/SVG validation
before reaching GPUI; opening a document never fetches remote content.
Caret and widget changes invalidate native IME coordinates; operating-system
CJK candidate placement remains a manual acceptance gate.

CI performance gates cover load/parse and source layout of 256 KiB fixtures.
Local measurements use the core `parse` and editor `layout` benches.

## Document model

- **On disk:** plain UTF-8 `.md` / `.txt` — no proprietary container.
- **`DocumentProcessingMode`:** `MarkdownWysiwyg` parses with comrak; `PlainText` skips parsing.
- **`revision`:** monotonic counter on every buffer edit; parse results carry the revision they were computed for so stale updates are ignored.
- **`saved_content`:** last reconciled disk snapshot (load, save, or last merged disk bytes). Used as the 3-way merge base.

## Parser strategy

| Layer | Engine | When |
|---|---|---|
| Document structure + source spans | comrak (same options as HTML export: GFM + footnotes + description lists + math_dollars + alerts + wikilinks) | Every edit, background thread |
| WYSIWYG tree | `rich::import` (comrak → `RichTree`) | On `RichEngine::sync` |
| Fenced-code highlighting | tree-sitter rust/json/yaml/bash | Viewport paint of a code body |

tree-sitter-md is not used. Source-mode `SyntaxNodeSpan`s are extracted from the comrak AST so masking cannot disagree with the rich tree's grammar. `==highlight==` is paired in that same pass (comrak has no highlight node). `$…$` / `$$…$$` are comrak `math_dollars` nodes (multiline `$$\n…\n$$` wrapping newlines and quote/list prefixes on those lines are dest chrome like `$` / `$$`). `[[wikilink]]` / `[[target|label]]` are comrak `wikilinks_title_after_pipe` nodes. GitHub/Typora `:smile:` shortcodes are paired from a modest alias table (unknown `:foo:` stays text). GitHub alerts (`> [!NOTE]`, …) are comrak `alerts` nodes. `[TOC]` / `[[toc]]` are classified on import as a TOC block (not a comrak node). YAML frontmatter may close with Jekyll/Pandoc `...` as well as `---` (comrak's delimiter is `---`; import rewrites the closer for parse and keeps original bytes). CommonMark indented-code sourcepos that drops the opening 4 spaces / tab is recovered so Home/click skip that indent. CommonMark 0–3 spaces before ATX `#` / a setext title / a fence / a thematic break are recovered the same way (quoted keep `>`; four spaces stay indented code). Fenced bodies strip `fence_offset` spaces on content lines.

## Editing surfaces

| Mode | Projection | Input ownership |
|---|---|---|
| WYSIWYG | Stable rich text; Markdown delimiters stay outside shaped text | Rich body or a focused widget draft |
| Source | Literal Markdown, uniform monospace rows, color-only syntax styling | Source editor |
| Split | Literal source on the left, editable rich text on the right | Last focused pane, remembered per tab |

**Show Markup Hints** controls a paint-only active-context tint, a non-interactive
floating syntax badge and a compact status-bar label such as `Bold · **…**`.
The badge is suppressed during selection, dragging and widget/table editing.
It never changes glyph advances,
line count, selection, document bytes or viewport anchoring. Native, Ocean
and Forest highlight palettes change colors only. Full syntax is available
in Source and Split. The older masking API remains internal, not a user mode;
see [projection policy](delimiter-masking.md).

Caret and selection use source byte offsets. Each rich leaf has a visible
glyph-to-source map and a separate logical caret ownership range. The latter
includes hidden delimiters and editable empty-list/paragraph endpoints.
This distinction prevents a valid EOF caret from disappearing merely because
its position has no visible glyph. Enter continues a nonempty list item;
Enter on an empty item exits to an editable paragraph.

Trailing spaces and tabs omitted by the Markdown AST are projected from exact
source bytes into paragraph/heading text leaves; plain paragraph leading spaces
and whitespace-only editable rows retain their glyph-to-source map. Typing
whitespace therefore moves the caret immediately. Structural table padding,
hard-break syntax and line endings remain hidden. First Enter creates an editable
paragraph; Enter in an already
empty plain paragraph is a no-op without an undo entry. List exit and literal
code/Source newlines retain their own behavior. Input, including a boundary
no-op, restarts the visible caret phase; idle blinking keeps IME geometry stable.
The EOF draft reserves the same separator spacing as its future paragraph,
preventing a vertical jump when its first non-whitespace character is entered.

Hidden delimiters are skipped by rich navigation and command targeting.
Tables show editable cell text without a persistent structural toolbar or raw
delimiter row. A floating row/column panel appears only for a focused, collapsed
caret inside a cell, never during selection or dragging. It uses freshly painted
caret geometry, stays inside the viewport and does not cover the active row or
participate in document layout. Format → Table remains available. Table
edits clamp to cells. Newly typed spaces at a trimmed cell boundary use `&#32;`
(ordinary U+0020, not a nonbreaking space), distinguishing authored content from
GFM alignment padding across parse, save/reopen and Undo/Redo. Existing padding
is not rewritten; internal spaces and literal Source input remain unchanged.
Widget drafts own their selection, IME origin
and undo while focused. Both panes repair stale UTF-8/grapheme endpoints
after shared-document edits. Unsupported active HTML is never executed.
Detailed syntax edge cases remain in [engineering notes](roadmap.md).

Split's passive `ShadowSelection` is separate from both input selections. The
workspace copies only the actually focused owner's source range, direction and
revision; the peer paints it through its native grapheme stops. Ghost updates
never enter Document, IME, undo/recovery or scroll-reveal state. Focus on a
palette, widget or image inspector suppresses the ghost; hidden/offscreen
syntax cannot force either pane to reveal markup or scroll.

Link destinations use an out-of-flow editor opened with Cmd-K, including when
markup hints are disabled. Enter or Tab commits the URL; Escape cancels the
draft. The rich label and document geometry stay unchanged during URL editing.
Save and Save As commit valid widget drafts before publishing document bytes.
Tab close commits valid drafts or archives an invalid draft separately. Window
close/Quit checkpoint raw uncommitted fields without requiring a save decision;
recovery reattaches them only to validated matching source targets.

Image properties use a separate bounded inspector with real plain-text input
entities. Local paths resolve from Document.path and preview crosses the same
approved image-cache boundary as body rendering. Apply is revision/source
pinned and atomic; Cancel discards the inspector only by explicit gesture.
Field focus/history/Paste are routed independently, and private recovery
stores a length-delimited URL/alt draft without publishing it to Markdown.

## Workspace chrome and recovery

Document tabs belong to the editor column, not the sidebar. A horizontally
scrollable strip reveals the active tab; dirty markers and full-path tooltips
distinguish drafts and same-named files. Welcome is limited to the initial
empty workspace. Recent files use basenames with full-path hover labels.
At compact widths, formatting tools scroll with explicit previous/next controls.

`recovery.rs` stores versioned private per-window snapshots; `Workspace`
observes buffer changes and checkpoints content, saved base, modes, selections
and active pane. Restoring compares current disk bytes against both the
saved base and the recovered buffer. A changed or missing target requires
an explicit save decision before publication. Recovery writes do not write the
original document.

Recovery files are owner-only on Unix, reject symlinks/hard links at their private
boundary, use lifetime window-owner leases and atomic replacement, and retain a
prior valid snapshot. Limits are 16 MiB serialized per window, 4 MiB per buffer/base,
256 total live/closed tab entries, and 64 recoverable windows. Dirty closed drafts
are retained, not
silently trimmed; archived named drafts reopen as separate Save As buffers.
Normal typing schedules checkpoints after a configured 25–150 ms without
postponing the first deadline during continuous input. Worker and fsync time
are additional; this is not a durability deadline. Normal Quit completes a
bounded synchronous final write; a slow disk may delay quitting. Our Quit and
titlebar-close commands keep unsaved state open on failure; a fully saved window
can close despite recovery metadata failure. GPUI's separate system
shutdown hook cannot veto an OS-requested exit. Errors and size limits are
visible, not silent. Clean closed windows retire their session; dirty closed
drafts remain recoverable. Recovery
does not restore undo history and cannot guarantee the last uncheckpointed
keystroke after abrupt termination or power loss. Native persistence tests
use isolated temporary stores, never the user's real session.
Uncommitted and invalid widget fields retain raw text and source identity.
Recovery never silently attaches a stale field to changed Markdown: it opens
the raw text as a pathless Source draft if exact reattachment is unsafe.
Finder/open events route to the last active live window. The receiver belongs
to the application, so closing the oldest window cannot orphan later events.

## Save and external edits

- Default save writes the buffer verbatim (untouched blocks are untouched bytes),
  fsyncs a unique sibling temporary file before rename, preserves existing
  permissions and syncs the parent directory on Unix.
- Save never offers normalization. Normalization is absent from the user-facing
  File menu; its internal, explicitly dispatched review remains available to
  engineering regression probes and is never part of ordinary Save.
- Background autosave writes private snapshots only, never the source file or
  its clean marker. Explicit Save is the publication boundary.
- Explicit save failures show a native error prompt and retain the dirty buffer.
  Document writes currently complete on the foreground executor; slow network
  or cloud-mounted volumes may delay editing. Moving that I/O off-thread needs
  revision-aware completion before it can safely mark a newer buffer clean.
- External edits use the last reconciled disk bytes, live buffer and fresh disk
  bytes for source-preserving three-way merge. Independent lines and bounded
  same-line grapheme edits merge; ambiguous/structural overlaps require review.
  Similar's structured regions avoid conflict-marker parsing and AST rewriting.
- Manual Save checks disk even without a watcher event. Atomic writes compare
  expected bytes before staging and again before rename; a final check/rename
  race with an uncooperative writer remains, not a portable compare-and-swap.
- Conflicts block source publication, not private checkpoints, including across
  recovery. Review tokens pin tab
  identity, revision and all three versions, then recheck disk before applying.
  Keep Mine, Use Disk and Keep Both retain the other version as a pathless draft
  with a durable checkpoint before buffer replacement. Cancel/stale tokens
  leave both versions intact. Clean tabs accept fresh disk bytes without reload.

## Build identity

The workspace manifest owns the product version. User-facing feature updates
advance the minor version rather than reuse the previous version; the current
release-update feature advances to `0.8.0`; the following `0.8.1` patch is the
first intended GitHub update-test release.

`markrust-app/build.rs` embeds a seconds-resolution UTC timestamp at compilation.
About MarkRust, the non-GUI `--build-info` JSON command and the macOS installer's
`MarkRustBuildDate` plist field all use that compiled identity. Packaging or
launching the app never invents a newer date. A no-op cached build retains its
timestamp; changes to product sources or manifests trigger a new stamp.

For reproducible builds, `SOURCE_DATE_EPOCH` supplies the timestamp instead of
the wall clock. About labels this provenance and JSON reports
`timestamp_source: "source-date-epoch"`. Invalid epochs fail the build. This
timestamp identifies a build, not its installation time or a unique content hash.

## Release-update boundary

`updater.rs` owns fixed-repository metadata, bounded HTTPS downloads, checksums
and safe archive/bundle validation. It never installs or executes the candidate.
`update_install.rs` prepares a private same-volume handoff and uses the current
trusted executable as a helper; pinned hashes and filesystem identities guard
replacement, rollback and the receipt. `update_ui.rs` owns the asynchronous
single-job state machine and explicit user actions. Application-level restart
checks all windows' dialog/composition guards and private checkpoints before
freezing input and quitting. None of these modules publish document files.

Keep this within the application crate for now: the repository, platform and
desktop lifecycle are product-specific. A separately published updater crate
would add a compatibility/security contract without helping this simple notepad.
See [in-app updates](deployment.md#in-app-macos-updates) for trust and acceptance.

## Related

- [WYSIWYG roadmap](roadmap.md) — phase status and handoff
- [Concurrent editing](concurrent-editing.md) — merge/save policy and verification gates
- [Everyday notepad](notepad-experience.md) — lifecycle, persistence boundaries and private session-crate proposal
- [Delimiter masking](delimiter-masking.md) — source-mode visibility algorithm
- [Apple macOS design guidance](https://developer.apple.com/design/human-interface-guidelines/designing-for-macos/) — familiar keyboard commands and configurable colors
