# Concurrent editing and save review

Implementation record, 2026-10-01. Ordinary Save must preserve Markdown bytes and
must not silently discard either side of a concurrent edit.

## Policy

- Use the last reconciled disk bytes as the three-way base, the live buffer as
  Mine, and freshly read disk bytes as Disk. Merge only unambiguous changes.
- Use Similar's structured upstream three-way merge, with bounded grapheme-level
  refinement for disjoint edits within a line. Keep CRLF, final-newline state,
  Markdown punctuation and code bytes; do not serialize through an AST.
- Conflicts pause autosave. A bounded in-app review shows Mine and Disk, with
  explicit Keep Mine, Use Disk, Keep Both and Cancel actions. Selecting a side
  retains the other as a pathless draft, checkpointed before replacement.
- Normal Save writes verbatim. Normalization is not exposed beside Save in the
  File menu; the internal explicit review is retained for regression probes,
  not automatic formatting. Review tokens bind document, revision and bytes.
- Save compares the expected disk bytes before staging and immediately before
  atomic replacement. A changed/deleted target blocks the write. Portable file
  replacement is not a compare-and-swap against uncooperative external writers;
  a final check/rename race remains an explicit limitation.
- Watcher results validate tab identity, path and fresh disk state. Cancel,
  stale review, read failure, or checkpoint failure cannot discard live edits.
- Explicit-save autosave fences survive recovery. Merge Undo retains the active
  pane's full selection; unchanged Save does not disturb selection or viewport.
- Interactive review is capped at 4 MiB per version. Unchanged large files
  remain saveable through chunked comparison with the known base; oversized
  concurrent replacements fail closed and require Save As to a new file.

## Scope and starting evidence

The checkout is dirty from the preceding UI improvement; those changes remain
in place. Starting commit: `fb6e9ea`. Only repository code and isolated test
files are in scope; the running installed app and the screenshot's document
are not modified.

| File | Starting working-file SHA-1 | Change |
|---|---|---|
| `crates/markrust-core/src/merge.rs` | `5098fa08cfdb20d6aa1fde9daae67ba5ce1ac357` | Replace lossy line merge; bounded conflict handling |
| `crates/markrust-core/src/document.rs` | `02c0dcbf14e2616185ba068d5d2a5ed123638094` | Checked atomic writes; undoable merge |
| `crates/markrust-app/src/workspace.rs` | `871a99109b8270e9feb7b14cabab7bc80c295284` | Save/recovery reconciliation and pinned review tokens |
| `crates/markrust-app/src/window.rs` | `276c2d51dcb441b04d4632faf2725eb0a26ec711` | Bounded scrollable review; no destructive banner click |

Dependency manifests, headless session parity, menus and native regression
tests are supporting changes. No commit, push or installation is implied.

## Verification and remaining gates

Test disjoint paragraphs and same-line/table cells, overlapping edits, repeated
lines, Unicode/graphemes, CRLF and final newlines. Test stale disk at Save,
autosave, review and before publication; failed writes, missing targets, stale
tab identity, preserved versions and undo. Native fixtures must prove ordinary
Save has no normalization modal and review body scrolls while controls remain
inside the viewport in both themes and compact sizes.

Run full Rust tests, formatting, strict clippy, native geometry/pixel comparison
and release build. Real AppKit dialogs, operating-system IME/VoiceOver, slow
volumes, multi-window coordination and abrupt interruption remain separate
acceptance gates. Update implementation evidence after checks, not beforehand.

## Implementation evidence

- [Similar 3.2.0](https://docs.rs/similar/3.2.0/similar/merge/struct.TextMerge.html)
  supplies structured diff3 regions. Exact-range and bounded
  extended-grapheme refinement handles independent adjacent/same-line edits;
  ambiguous repeated tokens and structural overlaps remain explicit conflicts.
- Normal Save uses checked atomic writes; private draft checkpoints never
  publish source files. Headless parity tests
  cover stale disk and non-destructive reload; native fixtures exercise actual
  Save/menu/key, review focus, scrolling, recovery and losing-version retention.
- The review is a GPUI overlay with fixed header/footer, virtualized body rows,
  separate horizontal/vertical scrolling and inline stale-version errors.
  Background editor commands, drops and native text insertion are blocked.
- Final local validation passed: 1,332 Rust tests with all features, strict
  workspace/all-target/all-feature Clippy, formatting and diff checks; 72 native
  journeys, 50 GUI states, 32 approved screenshot comparisons and the six new
  reconciliation families. Website checks passed 3 unit and 12 browser tests.
- The new fixtures verify Normalize Apply is buffer-only through debounce and
  Undo restores original bytes/focus. Native sink registration and callback
  no-ops are checked; real operating-system IME injection remains a manual gate.
- Metal screenshots and review-state/bounds JSON are in the local ignored
  `target/gui-concurrent-editing-final/` directory. Compact Light/Dark images
  were inspected; no baseline update was needed for this change.
- Release build and `--version` passed. Binary SHA-256:
  `c7c20a29d4357c8942e2daaf27d8f88365973ef5373194cdb2a122d6b8619365`.
  The running installed app was not replaced or restarted; open drafts require
  a user-confirmed safe handoff before installation. Changes remain local.

## Related

- [Architecture](architecture.md)
- [Native GUI testing](gui-testing.md)
- [Product roadmap](../ROADMAP.md)
- [Everyday notepad](notepad-experience.md) — draft-only persistence and window lifecycle
