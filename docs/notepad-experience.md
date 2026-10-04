# Everyday notepad experience

Implementation record, 2026-10-01. MarkRust should be a dependable document
editor, not an IDE. No accounts, cloud sync, Git UI or public library release
is required for this work.

The latest updater follow-up is recorded under
[version 0.8](#version-08-guarded-macos-release-updates).

## Version 0.8 guarded macOS release updates

The app checks the fixed GitHub repository for newer stable versions on a
background thread. The persisted automatic-check setting can be changed live;
manual checks, repository navigation and explicit download/restart actions are
available in the application menu. No source text or document paths are sent.
The release loader rejects untrusted URLs, wrong architecture/version, corrupt
checksums, unsafe archives and invalid bundles before preparing replacement.

Update restart retains the ordinary all-window private recovery guarantee.
Open dialogs and text compositions block it; checkpoint failure cannot close
the app. A helper copied from the existing executable waits for parent exit,
revalidates target/stage identities, atomically swaps bundles and retains the
previous app and a private receipt. A quit veto cancels the helper before input
is unfrozen. The live recovery lease now retains its inode through checkpoint
rotation, preventing a competing window from claiming an active session.

The local `0.8.0` seed and next `0.8.1` public release are the intended update
test pair. Native fixture checks use synthetic stores and cover restart guards,
both failed and successful two-window checkpoints, and light/dark translated
notices without changing document geometry, focus, selection or viewport.
Actual installation, remote publication and draft-restoring startup are separate
acceptance results, not inferred from these fixtures. See
[deployment](deployment.md#in-app-macos-updates) for safety and signing limits.

### Local seed evidence, 2026-10-04

- Workspace all-feature tests: 1,503 passed; strict all-target/all-feature Clippy,
  formatting, 16 packaging contracts, pinned-workflow actionlint, three website
  unit tests, 16 Chromium tests and the 25-page production build passed.
- The fresh Metal run matched all 32 reviewed references and passed both themes'
  262 journeys plus existing native recovery/navigation states. Its new notice
  probe initially compared client coordinates to a desktop-positioned window;
  that test-only error was corrected. A separate Metal run passed all 12 update
  states, including real Later clicks. Evidence: `target/auto-update-native-seed.log`
  and `target/auto-update-native-isolated.log`. The complete CI-style geometry
  gate is recorded separately, rather than calling the interrupted first run a pass.
  That complete run passed 109 states and 262 journeys with screenshots explicitly
  disabled: `target/auto-update-native-geometry.log` and
  `target/gui-auto-update-seed-geometry`.
- A real Python-packaged, ad-hoc signed `0.8.0` archive passed the production
  bounded extractor, tar/PAX handling, plist/Mach-O validation and strict codesign
  checks without execution or installation. Evidence:
  `target/auto-update-real-archive-probe.json`; SHA-256
  `561354c6e6565574b0859268fee2c66b17ee023ba76cd7f678fef1d609b6a9b1`.
- With the user's explicit approval and the old app closed, `/Applications/MarkRust.app`
  was replaced by `0.8.0`, built at `2026-10-04T17:59:46Z`, signature-verified and
  launched (PID 32489 observed). The previous `0.5.0` bundle remains at
  `/Applications/.MarkRust-install.qbrALa/Previous-MarkRust.app`.
  The installer did not edit document/recovery data; ordinary startup resumed
  the user's recovered session. Evidence: `target/auto-update-seed-install.log`.

### Verified `0.8.1` source and publication gate

The tagged source is `7da9ed99ea959d556fa75f43385374efde9ccbd9`.
It passed 1,508 all-feature Rust tests, strict default and all-feature Clippy,
formatting, 16 packaging contracts and the exact `v0.8.1` tag guard.
The complete local Metal run passed 109 native states and 262 journeys,
matching all 32 reviewed screenshots without updating references. Evidence:
`target/auto-update-081-retirement-rust.log`,
`target/auto-update-081-retirement-default-clippy.log`,
`target/auto-update-081-retirement-all-clippy.log`, and
`target/auto-update-081-retirement-metal.log`.

[CI 37227957784](https://github.com/alexey-a-abramov/markrust/actions/runs/37227957784)
succeeded on that exact source, including the minimum toolchain, website,
native macOS GUI gate, and all four native test/build/smoke/package/upload jobs.
The Windows recovery-retirement regression passed on Windows: retirement now
closes the shared lease before cleanup, rejects retained-worker writes, and
cannot recreate a retired session. Unix checkpoints retain their locked inode.
All four development archives' checksums and source/version/target manifests
were reviewed. The ARM archive also passed the production bounded extractor,
plist/Mach-O and strict signature probe without execution or installation:
`target/auto-update-ci081-final-arm-probe.json`.

The immutable `v0.8.1` tag resolved to that same verified commit.
[Release 37230381296](https://github.com/alexey-a-abramov/markrust/actions/runs/37230381296)
passed all fresh verification gates and native builds, then published
[v0.8.1](https://github.com/alexey-a-abramov/markrust/releases/tag/v0.8.1)
at `2026-10-04T20:33:13Z`. GitHub's latest-release endpoint reports a stable,
non-draft release with all eight archive/checksum assets uploaded.

The actual published ARM archive was downloaded separately and passed the same
production extraction, identity/architecture and strict signature probe without
execution or installation. Its manifest identifies the tagged source and build
`2026-10-04T20:20:45Z`; SHA-256
`d795e51cf2b0c19cb97e665dfdfa1243e4bef1f606c91bcc60ef867c2bea9158`.
Evidence: `target/auto-update-published081-arm-probe.json`,
`target/auto-update-release081-latest.json` and
`target/auto-update-release081-final-run.json`.

The publication documentation/download links passed three website unit tests,
16 Chromium E2E tests and an isolated 25-page build. A concurrent build collided
with E2E's output regeneration; its sequential rerun passed. Run these commands
sequentially because they share `website/dist`. Evidence:
`target/auto-update-publication-website-test.log` and
`target/auto-update-publication-website-build-final.log`. The website itself was
not deployed, and no DNS or repository setting was changed.

Actual updater-driven startup remains pending: the Mac was locked when native
interaction was requested. The installed `0.8.0` seed remains running with
three dirty tabs, including an unsaved control paragraph. Buffer/base hashes
were captured privately without exporting user text. Continue through the app's
Check for Updates, Download Update, and Restart and Update controls only after
unlocking; verify the new process, About identity, retained backup/receipt and
exact recovered buffers. Do not replace the live bundle or force-quit the app.

## Version 0.7.0 navigation, locations and live UI language

Find uses a real auxiliary native text field, Cmd/Ctrl-F, next/previous shortcuts,
match counts and passive highlights mapped through each editor's actual glyph
rows. Query editing never changes the document or body history. Only the
originating pane follows match navigation; dismissal with a result selects the
visible result, while no-results/empty queries restore the original context.
Native query clipboard and Undo are isolated from document editing. Pending
widget/image/composition drafts retain ownership rather than being discarded.

File → Open Location accepts an existing file or folder path, including relative,
quoted and `~/` paths. The displayed base is the open folder, otherwise the
active file's directory, then Documents/current directory. Canonical validation
keeps errors in the modal; successful opens reuse existing file/folder routing
and preserve dirty tabs. Header paths, named folder roots, parent-directory
captions and a separate outside-folder group clarify location without adding
project or IDE concepts. Basename recent rows retain full-path hover tooltips.

View → Language changes common menus/chrome immediately in existing windows and
the configuration for future ones. The 20 embedded catalogs share 194 checked
keys; documents, paths and syntax are not translated. Locale-only updates never
remeasure content or request a caret reveal, even when the old caret is outside
a manually scrolled viewport. Initial translations need native-speaker review;
unsupported diagnostics use English fallback. Arabic/Hebrew retain logical
Unicode order; full RTL chrome, OS menu, screen-reader and candidate-window IME
acceptance remain separate. Find/Open Location Escape defers to active IME
preedit rather than prematurely closing its field.

The vector icons share a consistent grid/stroke, with distinct inline/source/
code-block symbols and explicit H1/H2/H3/Body labels. Translated Body labels get
bounded intrinsic width instead of overlapping neighboring tools. Theme moves
to Cmd/Ctrl-Alt-Shift-T, leaving Cmd/Ctrl-Shift-T reserved for tab reopening.
Settings remain limited to language, appearance, highlight colors, hints and
panel visibility; replacement, zoom/focus mode and tab reopening are later work.

The prepared native-runner Actions matrix produces push artifacts for macOS
ARM/Intel, Linux and experimental Windows; version-tag Releases have separate
verification/publication gates. Signing, Windows recovery ACL/locking, native
cross-platform acceptance and the first remote run are not established by local
tests. See [binary distribution](deployment.md#binary-distribution).

### Local verification, 2026-10-04

- All-feature workspace Rust tests: 1,470 passed. Strict all-target/all-feature
  Clippy, formatting, and `git diff --check` passed.
- The reviewed native Metal run passed 97 GUI states and 262 action journeys
  across both themes. A fresh comparison then passed all 32 reviewed screenshot
  references without updating them, alongside the complete native state/journey
  gate. Evidence: `target/navigation-final-native.log` and
  `target/gui-navigation-final-verified`. The CI-style `--geometry-only` run
  also passed the complete 97-state/262-journey gate without screenshots:
  `target/navigation-geometry-native.log` and `target/gui-navigation-geometry-final`.
  Eight Open Location states cover native field focus,
  invalid paths, cancellation, file opening and a visible named folder sidebar.
  Sixteen live-language states cover RU/JA/AR/EN body and active Find chrome,
  including preserved manually scrolled viewports and unchanged real selection.
- Twenty isolated Find journeys also passed with the first three paint frames
  and settled screenshots recorded. These supplement, not replace, the full
  native gate. Evidence: `target/navigation-reviewed-native.log`,
  `target/gui-navigation-reviewed`, `target/navigation-find-frames.log` and
  `target/gui-navigation-find-frames`.
- Packaging/workflow contracts: 16 passed; `actionlint` and the exact `v0.7.0`
  tag guard passed. Website checks: 3 unit tests, 16 Chromium E2E tests and a
  25-page production build passed. Evidence: `target/navigation-locale-rust-final.log`,
  `target/navigation-locale-clippy.log`, `target/navigation-locale-website-final.log`
  and `target/navigation-locale-website-build-final.log`.
- The optimized macOS ARM64 binary and packaged CLI/app report `0.7.0`, built
  at `2026-10-04T14:39:57Z`. The extracted app passed strict ad-hoc signature
  verification and plist inspection; both executable modes are `0755`.
  Local archive: `target/navigation-package-v07/markrust-macos-aarch64.tar.gz`,
  SHA-256 `19fe087909176b36584eaf6b6991b8c7013d40631595f234f1cfc14f49d7259a`.
  Its supplied checkout SHA is packaging metadata, not proof that this dirty
  working-tree binary exactly corresponds to the committed source.

No installed app, user document or personal recovery store was replaced or
opened by these checks. No push, tag, release, signing-secret or repository-setting
mutation occurred. Native Windows/Linux acceptance, OS menus, full RTL layout,
real candidate-window IME and VoiceOver remain explicit follow-up gates.

## Approved behavior

- Finder/open-document events add or activate a tab in the most recently
  active editor window. Closed windows cannot own the open-event receiver.
- New Window is explicit; New Document and New Tab stay in the current window.
- Background persistence writes private drafts, never the source file. Only
  Save or Save As publishes a buffer to a document path.
- Ordinary close/quit does not ask whether to save the source. It first
  checkpoints drafts and records a nonblocking recovery notice. A failed
  checkpoint must not be presented as successful preservation.
- Restore retains unsaved text and the exact last reconciled disk base. A
  changed target goes through three-way merge/review before any explicit write.
- Invalid widget fields are data too: retain their raw text independently of
  the document. Reattach only to a validated matching source target; otherwise
  keep a separate recoverable draft rather than guessing.
- Windows need independent recovery identities; one window must not overwrite
  another window's drafts or repeatedly restore the same session.

## Safety boundaries

Owner-only Unix directory/file permissions protect against other ordinary
local users. They do not encrypt content or exclude administrators and other
applications running as the same user. Recovery is local Application Support
data, not a working-directory file or telemetry.

Atomic snapshots, a previous valid generation and synchronous graceful-exit
checkpoints reduce loss. Abrupt termination before a checkpoint, power loss,
storage exhaustion and an uncooperative external writer remain real limits.
The pinned GPUI system-quit callback cannot veto shutdown; our normal Quit and
titlebar-close paths can refuse to discard state when persistence fails.
Existing 4 MiB per-buffer, 16 MiB snapshot, 256-entry and 64-window limits remain explicit;
do not silently trim dirty drafts to fit them.

Merge uses Similar's structured diff3 with bounded grapheme refinement, not
AST normalization or an AI guess. Comrak already handles Markdown parsing.
Independent edits can combine; ambiguous or overlapping edits require review.
Keep the originals and the common base. See [concurrent editing](concurrent-editing.md).

## Architecture recommendation

Keep `markrust-core` as the GUI-free document/Markdown engine. Keep projections,
caret geometry and widgets in `markrust-editor`, and OS menus/window lifecycle
in `markrust-app`.

The next useful extraction is a **private workspace `markrust-session` crate**:
recovery schema/storage, tab/window identities, reconcile decisions and
revision-pinned persistence commands. It should not depend on GPUI. Use one
state-transition implementation from both the native adapter and headless
journeys; their current duplicated save/restore policy can drift. First extract
the already pure recovery module and routing policy, then migrate transitions
behind the existing regression suite. Move editor-snapshot conversions to the
native adapter first: recovery schema/storage must not import editor view types.
Do not combine this mechanical refactor
with new merge semantics or publish it to crates.io.

Persistence completions must carry window/tab identity, document revision and
expected disk base. The model emits effects; adapters perform I/O and return
stamped results. Observations inspect the live model and actual painted scene,
not a second mock editor. Screenshot tests complement state invariants rather
than replace them.

## Small-editor priorities

| Priority | Feature | Boundary |
|---|---|---|
| Implemented locally | Find, Cmd/Ctrl-F and Cmd/Ctrl-G | Literal current-document search; passive source-addressed highlights |
| Later | Replace | Current document only; explicit, safe Undo |
| Next | Reopen closed tab, Cmd-Shift-T | Recover original draft; move theme shortcut |
| Next | Clear saved/recoverable/error status | Never imply source bytes were saved by private checkpoint |
| Later | Focus mode and text zoom | Hide chrome; no new workspace concepts |
| Later | Print / PDF, spelling and accessible keyboard navigation | Native document tools, not project tooling |
| Later | Bounded local version history | Explicit retention/export; no silent dirty-draft expiry |

### Optional future choices

- **Question:** Should private recovery also be encrypted with a macOS
  Keychain-managed key? Recommendation: defer until key recovery, locked-session
  behavior and migration failures have their own tested design.
  **User decision:** _[Enter your decision here]_
  **Notes (optional):** _[Enter comments or conditions here]_
- **Question:** Should named documents ever opt into source-file autosave?
  Recommendation: keep it off; private draft autosave is sufficient for this
  notepad workflow.
  **User decision:** _[Enter your decision here]_
  **Notes (optional):** _[Enter comments or conditions here]_

## Verification

Local verification on 2026-10-01 (not a CI or distribution release):

- `cargo test --locked --workspace --all-features`: 1,355 passed.
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`,
  `cargo fmt --all -- --check`, `git diff --check`, installer `bash -n` and
  ShellCheck passed.
- Full native runner: 72 action journeys, 57 native states, 32 approved Metal
  image comparisons passed without updating references. The seven new notepad
  checks cover window routing, queued external opens, raw widget/body preedit,
  restart, stale-target scratch, closed drafts and failed storage. All stores
  and documents in this runner are synthetic and isolated.
- Evidence: `target/notepad-rust-all-features.log`, `target/notepad-clippy.log`,
  `target/notepad-native-final.log`, and `target/gui-notepad-final`.
- Release build and installed CLI both report `0.1.0`. Installed bundle:
  `/Applications/MarkRust.app`, build date `2026-10-01T11:07:47Z`; strict ad-hoc
  signature verification passed. This is not a notarized distribution build.
- Raw release SHA-256:
  `93ecf75ef09d7b114848719e42874207a1caa71a2c00240e7ec1149112d62afb`.
  Installed executable SHA-256 after bundle signing:
  `699c36abcc4dab6abb7378bf67b2f6c25f4bf65ef5ff33191195740a5d31c926`.
- Prior verified bundle retained at
  `/Applications/.MarkRust-install.HaQOO7/Previous-MarkRust.app`. Installation
  did not replace recovery data or edit existing user documents.
- Live macOS checks: the existing Markdown default resolves to the installed
  bundle; Finder opened two synthetic `.md` files as neighboring tabs. File →
  New Window created a second window, and the next Finder open went to that
  window. Closing the synthetic windows and reopening left one blank window.

Real OS candidate-window IME and VoiceOver acceptance remain manual gates;
direct input-handler tests do not prove those platform interactions. Modified
keyboard delivery through the UI helper was inconclusive, so the live smoke
does not certify every physical shortcut. A transient helper-service crash
during window inspection was recovered; MarkRust stayed running.

### Version 0.2.0 follow-up

On 2026-10-01, the workspace advanced to `0.2.0`. About now displays the
compiled UTC build timestamp; CLI and installer use the same identity. See
[build identity](architecture.md#build-identity).

- All-feature Rust tests: 1,360 passed. Strict all-target Clippy, formatting,
  installer syntax/ShellCheck, website unit tests (3) and website build passed.
- Installed `/Applications/MarkRust.app`: `0.2.0`, compiled timestamp
  `2026-10-01T11:38:52Z`. Live native About, installed `--build-info` and bundle
  metadata matched. Strict ad-hoc signature verification passed.
- Previous verified bundle retained at
  `/Applications/.MarkRust-install.KsIfsA/Previous-MarkRust.app`. Only the empty
  synthetic test window was closed for installation; recovery data and user
  documents were not replaced. The updated editor was reopened and left ready.
- Evidence: `target/build-info-tests-final.log`,
  `target/build-info-clippy-final.log`, `target/build-info-install.log`,
  `target/build-info-website-tests.log` and `target/build-info-website-build.log`.
  This is local validation and installation, not CI, a published release or
  a new full native-render baseline run.

### Version 0.3.0 input and table follow-up

On 2026-10-01, ordinary Save remained byte-preserving and normalization was
removed from the File menu. External-change reconciliation still protects disk
and local edits; its conflict review is not a normalization prompt. Table cells
now remain clean while editing; explicit structural commands live in
Format → Table rather than a persistent document toolbar.

Whitespace has immediate native caret advances, including authored boundary
spaces in table cells. An empty EOF paragraph reserves its future text baseline;
its first letter no longer moves down. Repeated Enter in an empty plain paragraph
preserves source, selection, revision and Undo. Body, Source and widget input
wake the caret, including Undo/Redo and boundary no-ops. See
[editing surfaces](architecture.md#editing-surfaces) for the exact projection
and table-space serialization policy.

- All-feature Rust tests: 1,382 passed. Strict all-target/all-feature Clippy,
  formatting, diff checks, installer syntax/ShellCheck, website unit tests (3)
  and website build passed.
- Native Metal run: 57 GUI states and 88 journeys, with every action frame
  recorded in both themes. All 32 approved PNG references matched byte-for-byte;
  no references were regenerated. New probes join typed input, logical caret
  stops, actual painted cursor quads, idle phases and paragraph baselines.
- Installed `/Applications/MarkRust.app`: `0.3.0`, compiled timestamp
  `2026-10-01T12:44:59Z`. Live About, installed CLI identity and bundle metadata
  matched. File menu has no normalization action; Format → Table exposes the
  structural operations. Strict ad-hoc signature verification passed.
- Previous bundle retained at
  `/Applications/.MarkRust-install.EKAaq6/Previous-MarkRust.app`. Only the verified
  empty test window was quit. Recovery data and user documents were not replaced;
  the updated app was reopened and left ready.
- Evidence: `target/input-ux-tests-final.log`, `target/input-ux-clippy-final.log`,
  `target/input-ux-native-final.log`, `target/gui-input-ux-final/` (PNG frames,
  JSONL, reports and offline timelines), and `target/input-ux-install.log`.
  These are local checks and installation, not CI or a published release.

Real OS IME candidate-window and VoiceOver acceptance remain manual gates.
The legacy programmatic workspace save adapters still discard checked-save
errors; current native Save/Save As do not use those adapters. Consolidating
their error reporting is a separate follow-up.

### Version 0.4.0 pointer, modes and contextual controls

On 2026-10-02, Source became literal monospace Markdown in both standalone and
Split modes, with color-only highlighting. Source now registers actual mouse
input; rich drag endpoints come from the focused leaf's freshly painted caret
stops rather than unrelated paragraphs. Terminal glyphs, Cyrillic word drags,
reverse selection and click-then-Enter have native pointer regression fixtures.

Optional WYSIWYG syntax badges remain outside text layout and can be disabled
with View → Show Markup Hints. The table panel returns only while editing an
actual table cell with a collapsed selection; it hides during selection/drag.
Its row/column buttons preserve the input owner and use live targets. The
command palette now owns native query input and restores the previous pane
on dismissal, including duplicate-title tab activation by identity.

- All-feature Rust tests: 1,405 passed. Strict all-target/all-feature Clippy,
  formatting and diff checks passed. Website checks passed: 3 unit tests,
  12 Chromium journeys and a 25-page build.
- Full native Metal run: 57 GUI states and 136 action journeys passed in both
  themes, with every journey frame recorded (731 diagnostic PNG captures).
  The 32 intentional UI baselines were inspected and explicitly regenerated;
  a separate native capture matched their decoded pixels exactly. State
  assertions inspect the real model, input owner and painted scene, not a
  second mock editing engine.
- Release binary reports `0.4.0`, compiled `2026-10-02T00:04:33Z` (UTC).
  The installed `0.3.0` app remains untouched while it has an open edited buffer;
  this record does not claim installation, CI or a published release.
- Evidence: `target/pointer-ux-tests.log`, `target/pointer-ux-clippy.log`,
  `target/pointer-ux-native-verified.log`, `target/gui-pointer-ux-verified/`,
  `target/pointer-ux-baseline-update.log`, `target/pointer-ux-baseline-verify.log`,
  `target/pointer-ux-website-tests.log` and `target/pointer-ux-website-build.log`.

Continuous edge autoscroll while holding a drag still, rich wordwise
double-click dragging and widget-safe clicks in interparagraph margins remain
explicit follow-ups. OS IME candidate windows and VoiceOver still require
native-window acceptance; these automated checks do not establish perfection.

### Version 0.5.0 visible editing and gesture ownership

On 2026-10-04, the viewport correction retained a rich block's measured
visibility before invalidation. Missing cached bounds after deletion no longer
mean that the already visible edited block should be anchored at the top.
Both editors leave fully visible carets in place, even near pane edges;
genuinely offscreen carets still receive a bounded reveal adjustment.

Body selection now uses the handled press anchor and actual release position,
including reverse gestures and releases without a final MouseMove. Rich margin
presses resolve from fresh painted rows and respect actual widget/control
bounds. Floating Files and Outline panels own their presses instead of starting
a drag underneath them. Source clicks below its last row place the caret at EOF.
Intermediate MouseMove events still repaint the selection.

- `cargo test --locked --workspace --all-features`: 1,418 passed, including
  13 additional tests since 0.4.0. Strict all-target/all-feature Clippy,
  formatting and diff checks passed. Website checks passed: 3 unit tests,
  12 Chromium journeys and a 25-page build.
- Complete native Metal gate: 57 GUI states and 224 action journeys passed in
  both themes. All 32 existing approved image comparisons passed without
  updating references. The 88 added journeys inspect the first three native
  paints after each action, including manual scrolling, middle/edge deletion,
  margin presses and exact press/move/release endpoints. Existing recovery,
  concurrent-edit and window-lifecycle checks also passed with isolated stores.
- Release binary reports `0.5.0`, compiled `2026-10-03T23:30:27Z` (UTC;
  2026-10-04 02:30:27 in Moscow). Raw release SHA-256:
  `5100c5bdf3a856305beba1bc79bf91b5e6f3fda57e2ab0ebad945b39522ce187`.
  Preparation left the running installed `0.3.0` app untouched until the user
  finished editing and closed it. The subsequent local installation is recorded
  below; these checks do not represent CI, notarization or a published release.
- Evidence: `target/viewport-selection-rust-verified.log`,
  `target/viewport-selection-clippy-verified.log`,
  `target/viewport-selection-native-final-verified.log`,
  `target/gui-viewport-selection-final-verified/`,
  `target/viewport-selection-website.log` and
  `target/viewport-selection-release-final-build.log`.
- Focused Metal recordings in `target/gui-visible-deletion-recorded/` and
  `target/gui-selection-release-recorded/` passed 4 and 8 journeys respectively.
  Their first-paint and settled PNGs were inspected: visible deletion retains
  the row baseline, and final release selects the complete intended Cyrillic
  word in Source and WYSIWYG Split panes. Before/after pixel checks also retain
  the unaffected document regions above and below the edited row.

#### Local installation and launch

On 2026-10-04, after the user confirmed that MarkRust was closed,
`scripts/install-macos.sh --release --launch` successfully installed and opened
`/Applications/MarkRust.app`. Installed CLI identity and bundle metadata both
report `0.5.0`, compiled `2026-10-03T23:30:27Z`; strict ad-hoc signature
verification passed. The native window was observed with a restored named tab
and an unsaved Untitled tab. No document edits or reloads were performed during
verification, and the installer did not replace recovery storage.

The previous bundle is retained at
`/Applications/.MarkRust-install.21MESc/Previous-MarkRust.app` for rollback.
Installation evidence: `target/viewport-selection-install.log`.

Continuous stationary edge autoscroll, rich wordwise double-click dragging,
widget-draft release endpoints and revision provenance for multiple structural
edits before one paint remain separate follow-ups. OS IME and VoiceOver
acceptance remain manual; the automated suite does not certify those layers.

### Version 0.6.0 linked context and editing affordances

- Split shows a muted, nonblinking dashed cursor with a cap and translucent
  selection in the other pane. It projects the actual input owner's Markdown
  byte range through native glyph layout, without moving the peer's real
  caret/selection, requesting scroll, revealing markup, or entering recovery
  history. Palette/widget input and an open image inspector suppress it; standalone
  modes have no shadow. Offscreen context remains offscreen.
- The formatting strip uses literal `H1`, `H2`, `H3` and `Body` labels. Body
  means a plain paragraph and retains Cmd-Alt-0. Native button clicks check
  their actual Markdown result, not only shortcut dispatch.
- A bare typed hyphen stays literal until a following space requests a list.
  Immediate Backspace cancels that automatic formatting to a literal marker;
  its transient cancellation record expires on other edits/navigation.
  Leaving a list must retain a visible plain-paragraph caret and separators
  when the user clicks the blank row and resumes typing. Middle-list drafts
  reserve the future paragraph geometry before its first character; resumed
  character-by-character typing and its separator splices form one Undo burst.
- Image clicks open a bounded inspector with location and alt-text fields,
  same-folder choices, native file selection, preview, Apply and Cancel.
  Paths resolve relative to the Markdown document; choosing a nearby file
  retains a relative reference. Preview uses the approved raster/SVG cache;
  remote requests remain opt-in and reject unsafe/private origins. The
  incomplete `https://` placeholder is not a valid image location.
- Inspector fields remain private drafts until Apply. Their focus, Undo/Redo
  and Paste routes are independent of the document; tab return restores the
  last field. Private recovery retains both fields and reattaches only against
  identical original source, otherwise preserving a scratch draft. Rich-body
  clicks cannot steal field focus or append a paragraph behind the inspector.
  In Split, a focused Source pane still owns document Paste/Undo while the
  inspector is visible. Properties scroll at compact heights; title and
  Apply/Cancel remain fixed within the pane. Wheel events cannot scroll the
  document underneath. Rendered images fit narrow Split panes; missing-preview
  messages wrap within their preview area.
- Pointer caret stops in both panes use grapheme boundaries, preventing a
  hit inside a combining sequence or multi-codepoint emoji. Native shadow
  assertions join its marker to those independently observed glyph stops.
- This is a local development update. Installed-app replacement, public
  publication, OS IME/VoiceOver certification and Git push are separate steps.

Validation on 2026-10-04 (macOS arm64, native CoreText and Metal):

- Locked all-feature workspace tests: **1,447 passed**. Strict all-target,
  all-feature Clippy, formatting and whitespace checks passed.
- Full native gate: **73 GUI states**, **242 journeys** across both themes,
  and comparison against all **32 reviewed screenshot references** passed.
  Evidence: `target/gui-shadow-v06-final-verified` and
  `target/shadow-v06-native-final-verified.log`.
- Final isolated inspector gate: **16 states** passed with Metal, then again
  in geometry-only mode. Native wheel, input ownership, draft recovery and
  compact action bounds are asserted. Metal additionally observed **7,053
  fixture raster pixels** inside each theme's preview, independently of the
  cache approval flag. Evidence: `target/gui-image-v06-pixel-final` and
  `target/gui-image-v06-pixel-geometry`.
- Website: 3 unit tests, 12 Chromium tests and the 25-page build passed.
- Optimized CLI reports `0.6.0`, compiled build identity
  `2026-10-04T10:43:53Z` (UTC; 13:43:53 Moscow). About and `--build-info` use
  that same identity. The installed app, user documents and user recovery
  storage were not replaced or modified by these fixture runs.

## Related

- [Architecture](architecture.md)
- [Concurrent editing](concurrent-editing.md)
- [Native GUI testing](gui-testing.md)
- [Product roadmap](../ROADMAP.md)
