// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native regression probes for concurrent disk reconciliation.
//!
//! These checks deliberately use real GPUI windows and actions, but every
//! document and recovery store lives below the GUI runner's supplied output
//! directory. Test workspaces disable the native file watcher, so a passing
//! reconciliation path proves that an explicit Save reads the current disk
//! state instead of relying on watcher delivery.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{ensure, Context as _, Result};
use gpui::{
    point, px, size, AppContext, Bounds, Entity, ExternalPaths, FileDropEvent, Focusable,
    HeadlessAppContext, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Pixels,
    PlatformInput, WindowHandle,
};
use serde_json::json;

use crate::config::{AppConfig, ThemeChoice};
use crate::recovery::RecoveryStore;
use crate::window::{
    MarkRustWindow, NormalizeMarkdown, ReviewBounds, ShowSource, ShowSplit, ShowWysiwyg,
};
use crate::workspace::{EditorMode, ExternalResolution, Workspace};
use markrust_core::Document;

const WIDE_WIDTH: f32 = 960.;
const WIDE_HEIGHT: f32 = 760.;
/// The smallest supported full-review viewport. Keep this exact: a compact
/// modal must remain operable here instead of depending on a roomy fixture.
const COMPACT_WIDTH: f32 = 680.;
const COMPACT_HEIGHT: f32 = 420.;

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn theme_name(theme: ThemeChoice) -> &'static str {
    match theme {
        ThemeChoice::Light => "light",
        ThemeChoice::Dark => "dark",
    }
}

/// Execute one focused native probe for each concurrent-editing contract.
///
/// The return value contributes to the GUI runner's state count. Screenshots
/// are diagnostic evidence only: fixture paths and review data are deliberately
/// ephemeral, so they are never compared as golden baselines.
pub(crate) fn check(
    cx: &mut HeadlessAppContext,
    output: &Path,
    geometry_only: bool,
) -> Result<usize> {
    let root = output.join(format!(
        "concurrent-reconciliation-{}-{}",
        std::process::id(),
        FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root)?;

    check_normal_save_is_verbatim(cx, &root)?;
    check_disjoint_save_reconciles_without_watcher(cx, &root)?;
    check_conflicting_cmd_save_never_overwrites_disk(cx, &root, geometry_only)?;
    check_external_review_cancel_and_stale_token(cx, &root)?;
    check_external_resolution_restart_preserves_every_version(cx, &root)?;
    check_normalization_review_scrolls_a_large_table(cx, &root, geometry_only)?;

    println!(
        "PASS concurrent-reconciliation (verbatim Save, watcherless merge, conflict review, resolution recovery, bounded normalization review)"
    );
    Ok(6)
}

fn check_normal_save_is_verbatim(cx: &mut HeadlessAppContext, root: &Path) -> Result<()> {
    let original = "Title\n=====\n\n-   preserve these bytes\n\n| left | right |\n| --- | --- |\n| a | b |\n\nFinal editable paragraph.\n";

    let directory = fixture_directory(root, "verbatim-save-clean-wysiwyg")?;
    let path = directory.join("verbatim.md");
    std::fs::write(&path, original)?;
    with_fixture(
        cx,
        Some(path.clone()),
        None,
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
        |cx, window, workspace| {
            // A clean save must be invisible to active rich-text selection state.
            // Use a reversed range specifically, because preserving only its bounds
            // still makes subsequent Shift-navigation behave incorrectly.
            keystroke(cx, window, "cmd-down")?;
            keystroke(cx, window, "shift-left")?;
            let selection_before = rich_selection_and_focus(cx, window, workspace)?;
            ensure!(
                selection_before.0.start < selection_before.0.end
                    && selection_before.1
                    && selection_before.2,
                "fixture did not create a focused reversed rich selection: {selection_before:?}"
            );

            // Ordinary Cmd-S must not open the opt-in Normalize Markdown review or
            // rewrite source formatting merely because a normalized candidate exists.
            keystroke(cx, window, "cmd-s")?;
            ensure!(
                std::fs::read_to_string(&path)? == original,
                "ordinary Cmd-S normalized or otherwise changed source bytes"
            );
            ensure!(
                review_state(cx, window)?.is_none(),
                "ordinary Cmd-S opened a review instead of saving verbatim"
            );
            ensure!(
                active_content(cx, workspace) == original,
                "ordinary Cmd-S changed the in-memory source"
            );
            ensure!(
                rich_selection_and_focus(cx, window, workspace)? == selection_before,
                "clean Cmd-S moved, reversed, or unfocused the active rich selection"
            );
            Ok(())
        },
    )?;

    // The save contract is independent of the currently visible editing
    // surface. Capture the exact post-input bytes from each live owner and
    // assert that Cmd-S writes those bytes without opening a review or dialog.
    for (mode, label) in [
        (EditorMode::Wysiwyg, "wysiwyg"),
        (EditorMode::Source, "source"),
        (EditorMode::Split, "split"),
    ] {
        let directory = fixture_directory(root, &format!("verbatim-save-{label}"))?;
        let path = directory.join("verbatim.md");
        std::fs::write(&path, original)?;
        with_fixture(
            cx,
            Some(path.clone()),
            None,
            ThemeChoice::Dark,
            WIDE_WIDTH,
            WIDE_HEIGHT,
            |cx, window, workspace| {
                set_editor_mode(cx, window, mode)?;
                ensure!(
                    active_mode(cx, workspace) == mode,
                    "{label}: the requested editing mode did not become active"
                );
                keystroke(cx, window, "cmd-down")?;
                keystroke(cx, window, "x")?;
                let current = active_content(cx, workspace);
                ensure!(
                    current != original && active_dirty(cx, workspace),
                    "{label}: native typing did not create a dirty buffer"
                );
                ensure!(
                    std::fs::read_to_string(&path)? == original,
                    "{label}: typing changed disk before an explicit save"
                );
                ensure!(
                    review_state(cx, window)?.is_none() && !application_dialog_open(cx, window)?,
                    "{label}: ordinary editing opened a review or application dialog"
                );

                keystroke(cx, window, "cmd-s")?;
                ensure!(
                    std::fs::read_to_string(&path)? == current,
                    "{label}: Cmd-S did not write the current buffer verbatim"
                );
                ensure!(
                    active_content(cx, workspace) == current && !active_dirty(cx, workspace),
                    "{label}: Cmd-S changed the buffer or left it dirty"
                );
                ensure!(
                    review_state(cx, window)?.is_none() && !application_dialog_open(cx, window)?,
                    "{label}: Cmd-S opened a review or application dialog"
                );
                Ok(())
            },
        )?;
    }
    Ok(())
}

fn check_disjoint_save_reconciles_without_watcher(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "watcherless-disjoint-merge")?;
    let path = directory.join("merge.md");
    let base = "base\nkeep\n";
    std::fs::write(&path, base)?;
    with_fixture(
        cx,
        Some(path.clone()),
        None,
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
        |cx, window, workspace| {
            let ours = "base\nkeep\nlocal\n";
            replace_active_content(cx, workspace, ours)?;
            keystroke(cx, window, "cmd-down")?;
            let rich_before_merge = rich_selection_and_focus(cx, window, workspace)?;
            // The rich editing surface treats the terminal Markdown newline as
            // a block boundary, so its document-end caret is the last editable
            // byte rather than the raw source's trailing newline byte.
            let rich_end = ours.trim_end_matches('\n').len();
            ensure!(
                rich_before_merge == (rich_end..rich_end, false, true),
                "fixture did not place the active rich caret at the local buffer end: {rich_before_merge:?}"
            );
            let theirs = "remote\nbase\nkeep\n";
            let expected = match markrust_core::three_way_merge(base, ours, theirs) {
                markrust_core::MergeOutcome::Merged(merged) => merged,
                other => anyhow::bail!("fixture must create a disjoint merge, got {other:?}"),
            };
            // This intentionally bypasses notify. GUI test workspaces have no native
            // watcher, so Cmd-S must reconcile fresh bytes read at the save boundary.
            std::fs::write(&path, theirs)?;
            keystroke(cx, window, "cmd-s")?;

            ensure!(
                std::fs::read_to_string(&path)? == expected,
                "Cmd-S did not write the reconciled disjoint version"
            );
            ensure!(
                active_content(cx, workspace) == expected,
                "reconciled buffer differs from the checked disk write"
            );
            ensure!(
                !active_dirty(cx, workspace),
                "successful reconciled Save left the document dirty"
            );
            ensure!(
                review_state(cx, window)?.is_none(),
                "disjoint Save should reconcile directly, not require a conflict review"
            );

            // Reconciliation is an undoable merge command. Undo must restore the
            // local buffer and its active rich caret rather than reviving the inactive
            // Source pane's initial zero offset.
            keystroke(cx, window, "cmd-z")?;
            ensure!(
                active_content(cx, workspace) == ours,
                "Undo after a disjoint merge did not restore the local buffer"
            );
            ensure!(
                rich_selection_and_focus(cx, window, workspace)? == rich_before_merge,
                "Undo after a disjoint merge did not restore the focused rich caret"
            );
            Ok(())
        },
    )
}

fn check_conflicting_cmd_save_never_overwrites_disk(
    cx: &mut HeadlessAppContext,
    root: &Path,
    geometry_only: bool,
) -> Result<()> {
    for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
        let directory =
            fixture_directory(root, &format!("cmd-save-conflict-{}", theme_name(theme)))?;
        let path = directory.join("conflict.md");
        let base = "same line\n";
        let ours = "local version\n";
        let theirs = "external version\n";
        std::fs::write(&path, base)?;
        with_fixture(
            cx,
            Some(path.clone()),
            None,
            theme,
            COMPACT_WIDTH,
            COMPACT_HEIGHT,
            |cx, window, workspace| {
                replace_active_content(cx, workspace, ours)?;
                std::fs::write(&path, theirs)?;
                keystroke(cx, window, "cmd-s")?;

                ensure!(
                    std::fs::read_to_string(&path)? == theirs,
                    "a conflicting Cmd-S overwrote the external disk version"
                );
                ensure!(
                    active_content(cx, workspace) == ours && active_dirty(cx, workspace),
                    "a conflicting Cmd-S discarded or marked clean the local buffer"
                );
                let state = review_state(cx, window)?
                    .context("conflicting Cmd-S opened no external review")?;
                ensure!(
                    state.0 == "external" && state.1 == "side-by-side" && !state.3,
                    "conflicting Cmd-S did not present a clean external-change review: {state:?}"
                );
                let bounds = review_bounds(cx, window)?;
                assert_review_geometry(
                    &bounds,
                    state.1,
                    6,
                    COMPACT_WIDTH,
                    COMPACT_HEIGHT,
                    &format!("external review ({})", theme_name(theme)),
                )?;
                write_review_evidence(
                    &directory.join("external-review-open.json"),
                    COMPACT_WIDTH,
                    COMPACT_HEIGHT,
                    state,
                    &bounds,
                    review_scroll_offsets(cx, window)?.context("external review offsets")?,
                    review_visible_ranges(cx, window)?.context("external review ranges")?,
                )?;
                capture_review(
                    cx,
                    window,
                    &directory.join("external-review-open.png"),
                    geometry_only,
                )?;

                // The overlay must occlude both routed commands and direct native
                // input/drop events, rather than merely looking modal.
                assert_review_blocks_background_input(cx, window, workspace, &directory)?;
                ensure!(
                    std::fs::read_to_string(&path)? == theirs,
                    "blocked review input changed the external disk file"
                );
                keystroke(cx, window, "escape")?;
                ensure!(
                    review_state(cx, window)?.is_none(),
                    "Escape did not close the external review after blocked input"
                );
                let before_resumed_typing = active_content(cx, workspace);
                keystroke(cx, window, "x")?;
                ensure!(
                    active_content(cx, workspace) != before_resumed_typing,
                    "editor input did not resume after cancelling the review"
                );
                Ok(())
            },
        )?;
    }
    Ok(())
}

fn check_external_review_cancel_and_stale_token(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "review-cancel-stale")?;
    let path = directory.join("review.md");
    let base = "same line\n";
    let ours = "mine\n";
    let theirs = "theirs\n";
    std::fs::write(&path, base)?;
    with_fixture(
        cx,
        Some(path.clone()),
        None,
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
        |cx, window, workspace| {
            replace_active_content(cx, workspace, ours)?;
            std::fs::write(&path, theirs)?;
            keystroke(cx, window, "cmd-s")?;
            ensure!(
                review_state(cx, window)?.is_some(),
                "expected external review before Cancel"
            );
            keystroke(cx, window, "escape")?;
            ensure!(
                review_state(cx, window)?.is_none(),
                "Escape did not cancel the external review"
            );
            ensure!(
                active_content(cx, workspace) == ours && std::fs::read_to_string(&path)? == theirs,
                "Cancel changed either preserved version"
            );

            // A token pins the exact buffer revision and exact disk bytes. If either
            // changes while the review is open, a resolution must fail closed rather
            // than applying a decision to newer data.
            let tab_id = active_tab_id(cx, workspace)?;
            let review = workspace
                .update(cx, |workspace, cx| {
                    workspace.prepare_external_review(tab_id, cx)
                })?
                .context("expected a review token after cancelled conflict")?;
            let newer = "theirs changed again\n";
            std::fs::write(&path, newer)?;
            let result = cx.update_window(window.into(), |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.resolve_external_review(
                        &review,
                        ExternalResolution::KeepBoth,
                        window,
                        cx,
                    )
                })
            })?;
            ensure!(
                result.is_err(),
                "a stale external-review token resolved newer disk bytes"
            );
            ensure!(
                std::fs::read_to_string(&path)? == newer && active_content(cx, workspace) == ours,
                "stale-token rejection did not preserve both newer disk and local buffer"
            );
            Ok(())
        },
    )
}

fn check_external_resolution_restart_preserves_every_version(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    check_keep_mine_restart_blocks_autosave(cx, root)?;
    check_use_disk_restart_preserves_mine_copy(cx, root)?;
    check_keep_both_restart_preserves_mine_copy(cx, root)?;
    Ok(())
}

fn check_keep_mine_restart_blocks_autosave(cx: &mut HeadlessAppContext, root: &Path) -> Result<()> {
    let directory = fixture_directory(root, "keep-mine-restart")?;
    let path = directory.join("keep-mine.md");
    let store = RecoveryStore::new(directory.join("private-session"));
    let base = "base\n";
    let ours = "mine\n";
    let theirs = "disk\n";
    std::fs::write(&path, base)?;
    with_fixture(
        cx,
        Some(path.clone()),
        Some(store.clone()),
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
        |cx, window, workspace| {
            replace_active_content(cx, workspace, ours)?;
            std::fs::write(&path, theirs)?;
            keystroke(cx, window, "cmd-s")?;
            choose_external_resolution(cx, window, 1)?; // Cancel → Keep Mine.
            ensure!(
                active_content(cx, workspace) == ours && std::fs::read_to_string(&path)? == theirs,
                "Keep Mine changed disk before the user explicitly saved"
            );
            checkpoint_recovery(cx, workspace, &store)
        },
    )?;

    with_fixture(
        cx,
        None,
        Some(store),
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
        |cx, window, workspace| {
            ensure!(
                active_content(cx, workspace) == ours && active_dirty(cx, workspace),
                "Keep Mine did not restore as the dirty named buffer"
            );
            // Native typing plus a full debounce proves the persisted autosave fence
            // survives restart. The disk remains the external version until an
            // explicit Save (and a subsequent review decision) is made.
            keystroke(cx, window, "x")?;
            cx.advance_clock(Duration::from_secs(2));
            cx.run_until_parked();
            draw(cx, window)?;
            ensure!(
                std::fs::read_to_string(&path)? == theirs,
                "restored Keep Mine autosaved over the external disk version"
            );
            Ok(())
        },
    )
}

fn check_use_disk_restart_preserves_mine_copy(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "use-disk-restart")?;
    let path = directory.join("use-disk.md");
    let store = RecoveryStore::new(directory.join("private-session"));
    let base = "base\n";
    let ours = "mine\n";
    let theirs = "disk\n";
    std::fs::write(&path, base)?;
    let (window, workspace) = open_fixture(
        cx,
        Some(path.clone()),
        Some(store.clone()),
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
    )?;
    replace_active_content(cx, &workspace, ours)?;
    std::fs::write(&path, theirs)?;
    keystroke(cx, window, "cmd-s")?;
    choose_external_resolution(cx, window, 2)?; // Cancel → Keep Mine → Use Disk.
    assert_named_and_draft_versions(cx, &workspace, &path, theirs, ours, "Use Disk")?;
    ensure!(
        std::fs::read_to_string(&path)? == theirs,
        "Use Disk wrote to disk"
    );
    checkpoint_recovery(cx, &workspace, &store)?;
    close_fixture(cx, window, workspace)?;

    let (window, workspace) = open_fixture(
        cx,
        None,
        Some(store),
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
    )?;
    assert_named_and_draft_versions(cx, &workspace, &path, theirs, ours, "Use Disk restart")?;
    ensure!(
        std::fs::read_to_string(&path)? == theirs,
        "Use Disk restart changed disk"
    );
    close_fixture(cx, window, workspace)?;
    Ok(())
}

fn check_keep_both_restart_preserves_mine_copy(
    cx: &mut HeadlessAppContext,
    root: &Path,
) -> Result<()> {
    let directory = fixture_directory(root, "keep-both-restart")?;
    let path = directory.join("keep-both.md");
    let store = RecoveryStore::new(directory.join("private-session"));
    let base = "base\n";
    let ours = "mine\n";
    let theirs = "disk\n";
    std::fs::write(&path, base)?;
    let (window, workspace) = open_fixture(
        cx,
        Some(path.clone()),
        Some(store.clone()),
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
    )?;
    replace_active_content(cx, &workspace, ours)?;
    std::fs::write(&path, theirs)?;
    keystroke(cx, window, "cmd-s")?;
    choose_external_resolution(cx, window, 3)?; // Cancel → Keep Mine → Use Disk → Keep Both.
    assert_named_and_draft_versions(cx, &workspace, &path, theirs, ours, "Keep Both")?;
    ensure!(
        active_content(cx, &workspace) == ours,
        "Keep Both did not focus the separate local draft"
    );
    checkpoint_recovery(cx, &workspace, &store)?;
    close_fixture(cx, window, workspace)?;

    let (window, workspace) = open_fixture(
        cx,
        None,
        Some(store),
        ThemeChoice::Dark,
        WIDE_WIDTH,
        WIDE_HEIGHT,
    )?;
    assert_named_and_draft_versions(cx, &workspace, &path, theirs, ours, "Keep Both restart")?;
    ensure!(
        std::fs::read_to_string(&path)? == theirs,
        "Keep Both restart changed disk"
    );
    close_fixture(cx, window, workspace)?;
    Ok(())
}

fn check_normalization_review_scrolls_a_large_table(
    cx: &mut HeadlessAppContext,
    root: &Path,
    geometry_only: bool,
) -> Result<()> {
    for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
        let directory =
            fixture_directory(root, &format!("normalization-scroll-{}", theme_name(theme)))?;
        let path = directory.join("large-table.md");
        let markdown = large_markdown_table(900);
        std::fs::write(&path, &markdown)?;
        let (window, workspace) = open_fixture(
            cx,
            Some(path.clone()),
            None,
            theme,
            COMPACT_WIDTH,
            COMPACT_HEIGHT,
        )?;

        // Normalize is an explicit menu action; dispatch it through the real
        // GPUI action route rather than reusing the ordinary Cmd-S code path.
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_action(Box::new(NormalizeMarkdown), cx);
        })?;
        draw(cx, window)?;
        let state = review_state(cx, window)?.context("Normalize Markdown opened no review")?;
        ensure!(
            state.0 == "normalization" && state.1 == "changes" && state.2 > 0,
            "Normalize Markdown did not open the bounded Changes review: {state:?}"
        );
        let changes_bounds = review_bounds(cx, window)?;
        assert_review_geometry(
            &changes_bounds,
            state.1,
            4,
            COMPACT_WIDTH,
            COMPACT_HEIGHT,
            &format!("normalization changes ({})", theme_name(theme)),
        )?;
        let changes_ranges =
            review_visible_ranges(cx, window)?.context("missing Changes ranges")?;
        let changes_offsets =
            review_scroll_offsets(cx, window)?.context("missing Changes offsets")?;
        ensure!(
            changes_ranges[0].start == 0 && changes_ranges[0].end < state.2,
            "compact Changes pane did not virtualize the normalization diff: {changes_ranges:?}"
        );
        write_review_evidence(
            &directory.join("normalization-changes-open.json"),
            COMPACT_WIDTH,
            COMPACT_HEIGHT,
            state,
            &changes_bounds,
            changes_offsets,
            changes_ranges.clone(),
        )?;
        capture_review(
            cx,
            window,
            &directory.join("normalization-changes-open.png"),
            geometry_only,
        )?;

        // This is an actual pointer Wheel event over the tracked Changes list,
        // not a direct mutation of its scroll handle.
        scroll_review_pane(cx, window, changes_bounds.panes[0], -20_000.)?;
        let changes_after_ranges =
            review_visible_ranges(cx, window)?.context("missing Changes ranges after Wheel")?;
        let changes_after_offsets =
            review_scroll_offsets(cx, window)?.context("missing Changes offsets after Wheel")?;
        ensure!(
            changes_after_ranges[0].start > changes_ranges[0].start
                && changes_after_offsets[0].1 != changes_offsets[0].1,
            "native Wheel did not reach offscreen diff rows: before={changes_ranges:?}, after={changes_after_ranges:?}, offsets={changes_offsets:?}->{changes_after_offsets:?}"
        );
        ensure!(
            changes_after_ranges[0].end <= state.2,
            "diff virtual range escaped the source: {changes_after_ranges:?}"
        );
        write_review_evidence(
            &directory.join("normalization-changes-wheel.json"),
            COMPACT_WIDTH,
            COMPACT_HEIGHT,
            state,
            &changes_bounds,
            changes_after_offsets,
            changes_after_ranges,
        )?;
        capture_review(
            cx,
            window,
            &directory.join("normalization-changes-wheel.png"),
            geometry_only,
        )?;

        // Tab from the overlay group to Changes, then Side by Side, then
        // activate it. PageDown and Right are native keyboard events that must
        // drive both virtual source panes, including horizontal overflow.
        keystroke(cx, window, "tab")?;
        keystroke(cx, window, "tab")?;
        keystroke(cx, window, "enter")?;
        let side_by_side =
            review_state(cx, window)?.context("review closed while switching view")?;
        ensure!(
            side_by_side.1 == "side-by-side",
            "review did not switch to Side by Side"
        );
        let side_bounds = review_bounds(cx, window)?;
        assert_review_geometry(
            &side_bounds,
            side_by_side.1,
            4,
            COMPACT_WIDTH,
            COMPACT_HEIGHT,
            &format!("normalization side-by-side ({})", theme_name(theme)),
        )?;
        let side_before_ranges =
            review_visible_ranges(cx, window)?.context("missing side-by-side ranges")?;
        let side_before_offsets =
            review_scroll_offsets(cx, window)?.context("missing side-by-side offsets")?;
        ensure!(
            side_before_ranges[1].start == 0
                && side_before_ranges[1].end < markdown.lines().count()
                && side_before_ranges[2].end < markdown.lines().count(),
            "compact source panes did not virtualize the large table: {side_before_ranges:?}"
        );
        keystroke(cx, window, "pagedown")?;
        let side_after_page_ranges = review_visible_ranges(cx, window)?
            .context("missing side-by-side ranges after PageDown")?;
        let side_after_page_offsets = review_scroll_offsets(cx, window)?
            .context("missing side-by-side offsets after PageDown")?;
        ensure!(
            side_after_page_ranges[1].start > side_before_ranges[1].start
                && side_after_page_ranges[2].start > side_before_ranges[2].start
                && side_after_page_offsets[1].1 != side_before_offsets[1].1
                && side_after_page_offsets[2].1 != side_before_offsets[2].1,
            "native PageDown did not scroll both source panes: before={side_before_ranges:?}, after={side_after_page_ranges:?}"
        );
        keystroke(cx, window, "right")?;
        let side_after_right_offsets = review_scroll_offsets(cx, window)?
            .context("missing side-by-side offsets after Right")?;
        ensure!(
            side_after_right_offsets[1].0 != side_after_page_offsets[1].0
                && side_after_right_offsets[2].0 != side_after_page_offsets[2].0,
            "native Right did not horizontally scroll both source panes: before={side_after_page_offsets:?}, after={side_after_right_offsets:?}"
        );
        ensure!(
            side_after_page_ranges[1].end <= markdown.lines().count()
                && side_after_page_ranges[2].end <= markdown.lines().count(),
            "source virtual range escaped the large table: {side_after_page_ranges:?}"
        );
        write_review_evidence(
            &directory.join("normalization-side-by-side.json"),
            COMPACT_WIDTH,
            COMPACT_HEIGHT,
            side_by_side,
            &side_bounds,
            side_after_right_offsets,
            side_after_page_ranges,
        )?;
        capture_review(
            cx,
            window,
            &directory.join("normalization-side-by-side.png"),
            geometry_only,
        )?;
        if theme == ThemeChoice::Light {
            // From Side by Side, move through Cancel to the real Apply control.
            // Applying normalization is deliberately buffer-only: autosave stays
            // fenced until the user makes an explicit Save decision.
            keystroke(cx, window, "tab")?;
            keystroke(cx, window, "tab")?;
            keystroke(cx, window, "enter")?;
            ensure!(
                review_state(cx, window)?.is_none(),
                "Apply Normalization did not close the review"
            );
            ensure!(
                active_content(cx, &workspace) != markdown,
                "Apply Normalization did not change the in-memory buffer"
            );
            cx.advance_clock(Duration::from_secs(2));
            cx.run_until_parked();
            draw(cx, window)?;
            ensure!(
                std::fs::read_to_string(&path)? == markdown,
                "Apply Normalization autosaved despite its explicit Save boundary"
            );
            keystroke(cx, window, "cmd-z")?;
            ensure!(
                active_content(cx, &workspace) == markdown,
                "Undo after Apply Normalization did not restore the original bytes"
            );
            ensure!(
                rich_selection_and_focus(cx, window, &workspace)?.2,
                "Undo after Apply Normalization did not restore editable rich focus"
            );
        } else {
            keystroke(cx, window, "escape")?;
            ensure!(
                review_state(cx, window)?.is_none(),
                "Escape did not close Normalize review"
            );
        }
        ensure!(
            std::fs::read_to_string(&path)? == markdown,
            "normalization review changed the source file without explicit Save"
        );

        close_fixture(cx, window, workspace)?;
    }
    Ok(())
}

fn fixture_directory(root: &Path, label: &str) -> Result<PathBuf> {
    let directory = root.join(format!(
        "{label}-{}",
        FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&directory)
        .with_context(|| format!("create native fixture directory {}", directory.display()))?;
    Ok(directory)
}

fn open_fixture(
    cx: &mut HeadlessAppContext,
    path: Option<PathBuf>,
    recovery_store: Option<RecoveryStore>,
    theme: ThemeChoice,
    width: f32,
    height: f32,
) -> Result<(WindowHandle<MarkRustWindow>, Entity<Workspace>)> {
    // Do not use Workspace::open_document here: it intentionally opens a
    // folder/listing task for the real app, which would make a tiny isolated
    // native fixture retain its Workspace after the window closes. Loading the
    // file first and replacing the test workspace's placeholder document keeps
    // the exact path/saved-base semantics while preserving deterministic teardown.
    let initial_document = path.clone().map(Document::from_file).transpose()?;
    let initial_title = path.as_ref().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    let window = cx.open_window(size(px(width), px(height)), move |window, cx| {
        let workspace = cx.new(move |cx| {
            let mut workspace = if let Some(store) = recovery_store {
                Workspace::new_for_recovery_tests(
                    AppConfig {
                        theme,
                        ..AppConfig::default()
                    },
                    store,
                    window,
                    cx,
                )
            } else {
                Workspace::new_for_gui_tests(
                    AppConfig {
                        theme,
                        ..AppConfig::default()
                    },
                    window,
                    cx,
                )
            };
            workspace.sidebar_open = false;
            workspace.outline_open = false;
            if let Some(document) = initial_document {
                let tab = workspace
                    .tabs
                    .get_mut(workspace.active_tab)
                    .expect("new test workspace must have a placeholder tab");
                tab.document.update(cx, |current, cx| {
                    *current = document;
                    cx.notify();
                });
                if let Some(title) = initial_title {
                    tab.title = title;
                }
            }
            if let Some(tab) = workspace.active_tab() {
                window.focus(&tab.rich_view.read(cx).focus_handle(cx), cx);
            }
            workspace
        });
        cx.new(|cx| MarkRustWindow::new(workspace, cx))
    })?;
    let root = window.root(cx)?;
    let workspace = cx.read_entity(&root, |root, _| root.workspace.clone());
    if let Err(error) = draw(cx, window) {
        eprintln!("concurrent fixture failed while opening: {error:#}");
        let cleanup = close_fixture(cx, window, workspace);
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => {
                Err(error.context(format!("fixture open cleanup also failed: {cleanup:#}")))
            }
        };
    }
    Ok((window, workspace))
}

/// Always release the real window and workspace before returning a fixture
/// assertion failure. GPUI correctly panics on leaked entities; without this
/// boundary that panic would hide the useful reconciliation assertion that
/// caused it.
fn with_fixture(
    cx: &mut HeadlessAppContext,
    path: Option<PathBuf>,
    recovery_store: Option<RecoveryStore>,
    theme: ThemeChoice,
    width: f32,
    height: f32,
    check: impl FnOnce(
        &mut HeadlessAppContext,
        WindowHandle<MarkRustWindow>,
        &Entity<Workspace>,
    ) -> Result<()>,
) -> Result<()> {
    let (window, workspace) = open_fixture(cx, path, recovery_store, theme, width, height)?;
    let result = check(cx, window, &workspace);
    if let Err(error) = &result {
        eprintln!("concurrent fixture failed before cleanup: {error:#}");
    }
    let cleanup = close_fixture(cx, window, workspace);
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("fixture cleanup also failed: {cleanup:#}")))
        }
    }
}

fn close_fixture(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: Entity<Workspace>,
) -> Result<()> {
    cx.update_window(window.into(), |_, window, _| window.remove_window())?;
    drop(workspace);
    // Mirrors the main GUI runner: allow editor parse/recovery tasks that
    // retain an entity briefly to quiesce before constructing the next window.
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    Ok(())
}

fn draw(cx: &mut HeadlessAppContext, window: WindowHandle<MarkRustWindow>) -> Result<()> {
    for _ in 0..3 {
        cx.advance_clock(Duration::from_millis(35));
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_next_frame(cx);
            window.refresh();
            window.draw(cx).clear(cx);
        })?;
    }
    Ok(())
}

fn keystroke(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    key: &str,
) -> Result<()> {
    let key = Keystroke::parse(key)?;
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_keystroke(key, cx);
    })?;
    draw(cx, window)
}

fn scroll_review_pane(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    bounds: Bounds<Pixels>,
    delta_y: f32,
) -> Result<()> {
    // Hit the actual virtual list bounds, never an assumed centered body.
    let position = point(
        bounds.left() + bounds.size.width / 2.,
        bounds.top() + bounds.size.height / 2.,
    );
    cx.update_window(window.into(), |_, window, cx| {
        window.simulate_mouse_move(position, cx);
        window.dispatch_event(
            PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position,
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(delta_y))),
                modifiers: Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            }),
            cx,
        );
    })?;
    draw(cx, window)
}

fn replace_active_content(
    cx: &mut HeadlessAppContext,
    workspace: &Entity<Workspace>,
    content: &str,
) -> Result<()> {
    let content = content.to_string();
    workspace.update(cx, |workspace, cx| {
        let document = workspace
            .active_tab()
            .context("missing active fixture document")?
            .document
            .clone();
        document.update(cx, |document, cx| {
            let end = document.buffer.len_bytes();
            document.replace_range(0, end, &content);
            cx.notify();
        });
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}

fn active_tab_id(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> Result<usize> {
    cx.read_entity(workspace, |workspace, _| {
        workspace.active_tab().map(|tab| tab.id)
    })
    .context("missing active fixture tab")
}

fn active_content(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> String {
    cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .expect("native fixture active tab")
            .document
            .read(cx)
            .buffer
            .content()
    })
}

fn active_dirty(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> bool {
    cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .expect("native fixture active tab")
            .document
            .read(cx)
            .dirty
    })
}

fn active_mode(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> EditorMode {
    cx.read_entity(workspace, |workspace, _| {
        workspace
            .active_tab()
            .expect("native fixture active tab")
            .mode
    })
}

fn set_editor_mode(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    mode: EditorMode,
) -> Result<()> {
    cx.update_window(window.into(), |_, window, cx| match mode {
        EditorMode::Wysiwyg => window.dispatch_action(Box::new(ShowWysiwyg), cx),
        EditorMode::Source => window.dispatch_action(Box::new(ShowSource), cx),
        EditorMode::Split => window.dispatch_action(Box::new(ShowSplit), cx),
    })?;
    draw(cx, window)
}

fn application_dialog_open(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
) -> Result<bool> {
    let root = window.root(cx)?;
    Ok(cx.read_entity(&root, |root, _| root.test_has_application_dialog()))
}

fn review_state(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
) -> Result<Option<(&'static str, &'static str, usize, bool)>> {
    let root = window.root(cx)?;
    Ok(cx.read_entity(&root, |root, _| {
        root.test_review_state()
            .map(|(kind, display, lines, error)| (kind, display, lines, error.is_some()))
    }))
}

fn review_scroll_offsets(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
) -> Result<Option<[(f32, f32); 3]>> {
    let root = window.root(cx)?;
    Ok(cx.read_entity(&root, |root, _| root.test_review_scroll_offsets()))
}

fn review_visible_ranges(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
) -> Result<Option<[std::ops::Range<usize>; 3]>> {
    let root = window.root(cx)?;
    Ok(cx.read_entity(&root, |root, _| root.test_review_visible_ranges()))
}

fn review_bounds(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
) -> Result<ReviewBounds> {
    let root = window.root(cx)?;
    cx.read_entity(&root, |root, _| root.test_review_bounds())
        .context("review did not expose painted bounds")
}

fn assert_review_geometry(
    bounds: &ReviewBounds,
    display: &str,
    expected_controls: usize,
    viewport_width: f32,
    viewport_height: f32,
    context: &str,
) -> Result<()> {
    ensure!(
        bounds.controls.len() == expected_controls,
        "{context}: expected {expected_controls} review controls, got {}",
        bounds.controls.len()
    );
    assert_bounds_visible(
        bounds.shell,
        viewport_width,
        viewport_height,
        context,
        "shell",
    )?;
    assert_bounds_visible(
        bounds.body,
        viewport_width,
        viewport_height,
        context,
        "body",
    )?;
    assert_bounds_visible(
        bounds.footer,
        viewport_width,
        viewport_height,
        context,
        "footer",
    )?;
    ensure!(
        bounds.body.top() >= bounds.shell.top()
            && bounds.footer.bottom() <= bounds.shell.bottom()
            && bounds.body.bottom() <= bounds.footer.top() + px(1.),
        "{context}: review body/footer escaped the shell: shell={:?}, body={:?}, footer={:?}",
        bounds.shell,
        bounds.body,
        bounds.footer
    );
    for (index, control) in bounds.controls.iter().copied().enumerate() {
        assert_bounds_visible(
            control,
            viewport_width,
            viewport_height,
            context,
            &format!("control {index}"),
        )?;
        ensure!(
            control.left() >= bounds.shell.left()
                && control.right() <= bounds.shell.right()
                && control.top() >= bounds.shell.top()
                && control.bottom() <= bounds.shell.bottom(),
            "{context}: control {index} lies outside the review shell: {control:?}"
        );
    }
    match display {
        // `ReviewBounds::panes` deliberately exposes the virtual list's
        // content bounds. Its width may exceed the viewport (that is what the
        // horizontal-scroll test needs), so assert its painted vertical slice
        // is inside the fully-visible review body instead of falsely requiring
        // its offscreen content width to fit.
        "changes" => assert_pane_content_in_body(
            bounds.panes[0],
            bounds.body,
            viewport_width,
            context,
            "Changes pane",
        )?,
        "side-by-side" => {
            assert_pane_content_in_body(
                bounds.panes[1],
                bounds.body,
                viewport_width,
                context,
                "Before pane",
            )?;
            assert_pane_content_in_body(
                bounds.panes[2],
                bounds.body,
                viewport_width,
                context,
                "After pane",
            )?;
        }
        other => anyhow::bail!("{context}: unknown review display {other:?}"),
    }
    Ok(())
}

fn assert_pane_content_in_body(
    pane: Bounds<Pixels>,
    body: Bounds<Pixels>,
    viewport_width: f32,
    context: &str,
    label: &str,
) -> Result<()> {
    ensure!(
        pane.size.width > px(0.) && pane.size.height > px(0.),
        "{context}: {label} has no painted scroll surface: {pane:?}"
    );
    ensure!(
        pane.top() >= body.top() - px(1.)
            && pane.bottom() <= body.bottom() + px(1.)
            && pane.left() < px(viewport_width)
            && pane.right() > px(0.),
        "{context}: {label} has no visible clipped region inside review body: pane={pane:?}, body={body:?}"
    );
    Ok(())
}

fn assert_bounds_visible(
    bounds: Bounds<Pixels>,
    viewport_width: f32,
    viewport_height: f32,
    context: &str,
    label: &str,
) -> Result<()> {
    let left = f32::from(bounds.left());
    let right = f32::from(bounds.right());
    let top = f32::from(bounds.top());
    let bottom = f32::from(bounds.bottom());
    ensure!(
        right > left && bottom > top,
        "{context}: {label} has no painted area: {bounds:?}"
    );
    ensure!(
        left >= -1.
            && top >= -1.
            && right <= viewport_width + 1.
            && bottom <= viewport_height + 1.,
        "{context}: {label} is not fully viewport-visible: {bounds:?}, viewport={viewport_width}x{viewport_height}"
    );
    Ok(())
}

fn write_review_evidence(
    path: &Path,
    viewport_width: f32,
    viewport_height: f32,
    state: (&'static str, &'static str, usize, bool),
    bounds: &ReviewBounds,
    offsets: [(f32, f32); 3],
    ranges: [std::ops::Range<usize>; 3],
) -> Result<()> {
    let pane_names = ["changes", "before", "after"];
    let document = json!({
        "viewport": { "width": viewport_width, "height": viewport_height },
        "review": {
            "kind": state.0,
            "display": state.1,
            "diff_lines": state.2,
            "error": state.3,
        },
        "bounds": {
            "shell": json_bounds(bounds.shell),
            "body": json_bounds(bounds.body),
            "footer": json_bounds(bounds.footer),
            "controls": bounds.controls.iter().copied().map(json_bounds).collect::<Vec<_>>(),
            "panes": bounds.panes.iter().copied().map(json_bounds).collect::<Vec<_>>(),
        },
        "panes": pane_names.into_iter().enumerate().map(|(index, name)| json!({
            "name": name,
            "horizontal_offset": offsets[index].0,
            "vertical_offset": offsets[index].1,
            "visible_range": { "start": ranges[index].start, "end": ranges[index].end },
        })).collect::<Vec<_>>(),
    });
    std::fs::write(path, serde_json::to_vec_pretty(&document)?)
        .with_context(|| format!("write review evidence {}", path.display()))?;
    Ok(())
}

fn json_bounds(bounds: Bounds<Pixels>) -> serde_json::Value {
    json!({
        "left": f32::from(bounds.left()),
        "top": f32::from(bounds.top()),
        "right": f32::from(bounds.right()),
        "bottom": f32::from(bounds.bottom()),
        "width": f32::from(bounds.size.width),
        "height": f32::from(bounds.size.height),
    })
}

fn capture_review(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    path: &Path,
    geometry_only: bool,
) -> Result<()> {
    if geometry_only {
        return Ok(());
    }
    let screenshot = cx
        .capture_screenshot(window.into())
        .context("review screenshot capture failed")?;
    crate::visual_tests::save_screenshot(&screenshot, path)
        .with_context(|| format!("save review screenshot {}", path.display()))
}

#[derive(Debug, PartialEq, Eq)]
struct ActiveEditorState {
    content: String,
    revision: u64,
    tab_count: usize,
    rich_selection: std::ops::Range<usize>,
    rich_selection_reversed: bool,
}

fn active_editor_state(
    cx: &HeadlessAppContext,
    workspace: &Entity<Workspace>,
) -> ActiveEditorState {
    cx.read_entity(workspace, |workspace, cx| {
        let tab = workspace.active_tab().expect("native fixture active tab");
        let document = tab.document.read(cx);
        let rich = tab.rich_view.read(cx);
        ActiveEditorState {
            content: document.buffer.content(),
            revision: document.revision(),
            tab_count: workspace.tabs.len(),
            rich_selection: rich.selected_range.clone(),
            rich_selection_reversed: rich.selection_reversed,
        }
    })
}

fn assert_review_blocks_background_input(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    directory: &Path,
) -> Result<()> {
    let before = active_editor_state(cx, workspace);
    let root = window.root(cx)?;
    ensure!(
        cx.read_entity(&root, |root, _| {
            root.test_review_native_input_sink_registered()
        }),
        "review did not register its non-editable native input sink during paint"
    );
    keystroke(cx, window, "cmd-b")?;
    keystroke(cx, window, "cmd-n")?;

    // This falls on the occluding backdrop rather than a review button. A
    // mouse click must not reach the editor that is visually behind it.
    click_at(cx, window, point(px(6.), px(COMPACT_HEIGHT / 2.)))?;

    let drop_path = directory.join("must-not-open-behind-review.md");
    std::fs::write(&drop_path, "dropped behind review\n")?;
    let position = point(px(6.), px(COMPACT_HEIGHT / 2.));
    cx.update_window(window.into(), |_, window, cx| {
        let paths = ExternalPaths([drop_path].into_iter().collect());
        window.dispatch_event(
            PlatformInput::FileDrop(FileDropEvent::Entered { position, paths }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::FileDrop(FileDropEvent::Submit { position }),
            cx,
        );
        window.dispatch_event(PlatformInput::FileDrop(FileDropEvent::Ended), cx);
    })?;
    draw(cx, window)?;

    // Physical printable keys are owned by the review's key route. Test the
    // non-key native replacement boundary separately by calling the registered
    // entity sink directly; HeadlessAppContext intentionally does not expose
    // Window's private platform handler for direct injection.
    keystroke(cx, window, "x")?;
    cx.update_window(window.into(), |_, window, cx| {
        root.update(cx, |root, cx| {
            gpui::EntityInputHandler::replace_text_in_range(root, None, "ime", window, cx);
            gpui::EntityInputHandler::replace_and_mark_text_in_range(
                root,
                None,
                "marked-ime",
                Some(0..1),
                window,
                cx,
            );
            gpui::EntityInputHandler::unmark_text(root, window, cx);
        });
    })?;
    draw(cx, window)?;
    ensure!(
        review_state(cx, window)?.is_some(),
        "a blocked command dismissed the active review"
    );
    ensure!(
        active_editor_state(cx, workspace) == before,
        "a review allowed a command, pointer, drop, or native text input to mutate the document behind it"
    );
    Ok(())
}

fn click_at(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    position: gpui::Point<Pixels>,
) -> Result<()> {
    cx.update_window(window.into(), |_, window, cx| {
        window.simulate_mouse_move(position, cx);
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                position,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                position,
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
            }),
            cx,
        );
    })?;
    draw(cx, window)
}

fn rich_selection_and_focus(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
) -> Result<(std::ops::Range<usize>, bool, bool)> {
    cx.update_window(window.into(), |_, window, cx| {
        let workspace = workspace.read(cx);
        let tab = workspace.active_tab().expect("native fixture active tab");
        let rich = tab.rich_view.read(cx);
        (
            rich.selected_range.clone(),
            rich.selection_reversed,
            rich.is_focused(window),
        )
    })
}

fn choose_external_resolution(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    tabs_after_cancel: usize,
) -> Result<()> {
    for _ in 0..tabs_after_cancel {
        keystroke(cx, window, "tab")?;
    }
    keystroke(cx, window, "enter")?;
    ensure!(
        review_state(cx, window)?.is_none(),
        "external resolution did not close the review"
    );
    Ok(())
}

fn checkpoint_recovery(
    cx: &mut HeadlessAppContext,
    workspace: &Entity<Workspace>,
    store: &RecoveryStore,
) -> Result<()> {
    workspace.update(cx, |workspace, cx| workspace.flush_recovery(cx));
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    ensure!(
        store.load().snapshot.is_some(),
        "external-resolution fixture did not produce a durable recovery checkpoint"
    );
    Ok(())
}

fn assert_named_and_draft_versions(
    cx: &HeadlessAppContext,
    workspace: &Entity<Workspace>,
    named_path: &Path,
    disk: &str,
    mine: &str,
    context: &str,
) -> Result<()> {
    let versions = cx.read_entity(workspace, |workspace, cx| {
        workspace
            .tabs
            .iter()
            .map(|tab| {
                let document = tab.document.read(cx);
                (
                    document.path.clone(),
                    document.buffer.content(),
                    document.dirty,
                )
            })
            .collect::<Vec<_>>()
    });
    let named = versions.iter().any(|(path, content, dirty)| {
        path.as_deref() == Some(named_path) && content == disk && !dirty
    });
    let draft = versions
        .iter()
        .any(|(path, content, dirty)| path.is_none() && content == mine && *dirty);
    ensure!(
        named && draft,
        "{context} did not retain one clean disk tab and one dirty pathless local draft: {versions:?}"
    );
    Ok(())
}

fn large_markdown_table(rows: usize) -> String {
    let mut markdown = String::from("Large table\n===========\n\n| Row | Value |\n| --- | --- |\n");
    for row in 0..rows {
        // Intentionally wider than either compact side-by-side column, so the
        // Right-arrow test exercises real horizontal list scrolling too.
        markdown.push_str(&format!("| {row} | value-{row:04}-{} |\n", "x".repeat(96)));
    }
    markdown
}
