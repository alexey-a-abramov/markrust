// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native text shaping, layout, input, and Metal screenshot regression checks.
//!
//! This is an example executable because AppKit requires the process main
//! thread. It uses the production window and editors but in-memory documents
//! and explicit configuration, so it never loads or saves user preferences.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure, Context as _, Result};
use gpui::{
    point, px, size, AppContext, ClipboardItem, Entity, Focusable, HeadlessAppContext, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput, WindowHandle,
};
use image::{Rgba, RgbaImage};
use markrust_editor::outline_headings;
use markrust_editor::wysiwyg::PaintedLeafGeometry;

use crate::config::{AppConfig, ThemeChoice};
use crate::panels::{Panel, OUTLINE_WIDTH};
use crate::window::MarkRustWindow;
use crate::workspace::{EditorMode, Workspace};

const HEIGHT: f32 = 1040.;
const WIDTHS: [f32; 2] = [720., 1200.];
const FIXTURES: [(&str, &str); 3] = [
    (
        "paragraph",
        include_str!("../tests/visual-fixtures/paragraph.md"),
    ),
    ("lists", include_str!("../tests/visual-fixtures/lists.md")),
    ("table", include_str!("../tests/visual-fixtures/table.md")),
];

struct Options {
    output: PathBuf,
    baseline: Option<PathBuf>,
    update_baselines: bool,
    geometry_only: bool,
    filter: Option<String>,
    usecases_seed: Option<u64>,
    usecases_count: Option<usize>,
    record_frames: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut options = Self {
            output: PathBuf::from("target/gui-regression"),
            baseline: None,
            update_baselines: false,
            geometry_only: false,
            filter: None,
            usecases_seed: None,
            usecases_count: None,
            record_frames: false,
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--output" => options.output = args.next().context("--output needs a path")?.into(),
                "--baseline" => options.baseline = Some(args.next().context("--baseline needs a path")?.into()),
                "--filter" => options.filter = Some(args.next().context("--filter needs a fixture name")?),
                "--update-baselines" => options.update_baselines = true,
                "--geometry-only" => options.geometry_only = true,
                "--record-frames" => options.record_frames = true,
                "--usecases-seed" => {
                    options.usecases_seed = Some(
                        args.next()
                            .context("--usecases-seed needs a u64")?
                            .parse()
                            .context("--usecases-seed must be a u64")?,
                    );
                }
                "--usecases-count" => {
                    options.usecases_count = Some(
                        args.next()
                            .context("--usecases-count needs a usize")?
                            .parse()
                            .context("--usecases-count must be a usize")?,
                    );
                }
                _ => bail!("Unknown argument {arg}. Use --output PATH, --baseline PATH, --update-baselines, --geometry-only, --record-frames, --filter NAME, --usecases-seed N, or --usecases-count N."),
            }
        }
        ensure!(
            !options.update_baselines || options.baseline.is_some(),
            "--update-baselines requires an explicit --baseline directory"
        );
        ensure!(
            !options.geometry_only || options.baseline.is_none(),
            "--geometry-only cannot compare or update screenshots"
        );
        ensure!(
            !options.geometry_only || !options.record_frames,
            "--record-frames requires Metal screenshots; omit --geometry-only"
        );
        ensure!(
            options
                .filter
                .as_ref()
                .is_none_or(|f| FIXTURES.iter().any(|(name, _)| name == f)),
            "Unknown fixture filter"
        );
        Ok(options)
    }
}

/// Run on the main thread of a native macOS executable.
pub fn run(args: impl Iterator<Item = String>) -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "Native GUI regression currently requires macOS for AppKit text and Metal screenshots"
    );
    let options = Options::parse(args)?;
    std::fs::create_dir_all(&options.output)?;
    if options.update_baselines {
        std::fs::create_dir_all(options.baseline.as_ref().unwrap())?;
    }
    if let Some(baseline) = &options.baseline {
        if baseline.exists() {
            ensure!(
                std::fs::canonicalize(&options.output)? != std::fs::canonicalize(baseline)?,
                "Screenshot output and approved baseline directories must differ; otherwise capture would overwrite the evidence before comparison"
            );
        }
    }
    let platform = gpui_platform::current_platform(true);
    let geometry_only = options.geometry_only;
    let mut cx =
        HeadlessAppContext::with_platform(platform.text_system(), Arc::new(()), move || {
            if geometry_only {
                None
            } else {
                gpui_platform::current_headless_renderer()
            }
        });
    cx.update(|cx| {
        crate::app::load_bundled_fonts(cx);
        cx.bind_keys(crate::app::desktop_key_bindings());
    });
    let mut snapshots = 0;
    let mut checked_input = false;
    let mut checked_application_commands = false;
    for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
        for (name, markdown) in FIXTURES {
            if options.filter.as_ref().is_some_and(|filter| filter != name) {
                continue;
            }
            let (window, workspace) = open_fixture(&mut cx, markdown, theme)?;
            let case_result: Result<()> = (|| {
                if !checked_application_commands {
                    check_application_command_routing(&mut cx, window)?;
                    checked_application_commands = true;
                }
                for width in WIDTHS {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.resize(size(px(width), px(HEIGHT)));
                        window.bounds_changed(cx);
                    })?;
                    draw(&mut cx, window)?;
                    let label = format!("{name}-{}-{}", theme_name(theme), width as u32);
                    capture(&mut cx, window, &workspace, &label, &options, true)?;
                    snapshots += 1;
                }
                for width in WIDTHS {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.resize(size(px(width), px(HEIGHT)));
                        window.bounds_changed(cx);
                    })?;
                    draw(&mut cx, window)?;
                    validate_geometry(&geometry(&cx, &workspace), "resize-roundtrip")?;
                }
                if name == "paragraph" {
                    check_input_selection_and_modes(
                        &mut cx, window, &workspace, markdown, theme, &options,
                    )?;
                    check_source_horizontal_access(&mut cx, window, &workspace, theme, &options)?;
                    snapshots += check_source_selection_geometry(
                        &mut cx, window, &workspace, theme, &options,
                    )?;
                    snapshots += 3;
                    checked_input = true;
                }
                if name == "table" {
                    // An odd viewport width exercises pixel snapping at fractional
                    // table-cell sizes, independently of the even screenshot widths.
                    cx.update_window(window.into(), |_, window, cx| {
                        window.resize(size(px(777.), px(HEIGHT)));
                        window.bounds_changed(cx);
                    })?;
                    draw(&mut cx, window)?;
                    validate_geometry(&geometry(&cx, &workspace), "table-777")?;
                }
                if name == "paragraph" {
                    snapshots +=
                        check_responsive_shell(&mut cx, window, &workspace, theme, &options)?;
                    snapshots += check_format_toolbar(
                        &mut cx, window, &workspace, markdown, theme, &options,
                    )?;
                    check_tab_navigation_and_markup_hints(
                        &mut cx, window, &workspace, theme, &options,
                    )?;
                    snapshots += 1;
                }
                Ok(())
            })();
            cx.update_window(window.into(), |_, window, _| window.remove_window())?;
            drop(workspace);
            // Production autosave tasks briefly retain their workspace. The
            // fixtures have no file paths, so draining them cannot save a file.
            cx.advance_clock(Duration::from_secs(2));
            cx.run_until_parked();
            case_result?;
        }
    }

    // Use-case matrix: deterministic generated scenarios and curated journeys,
    // including the requested Enter-on-empty-task-list case.
    // Each journey declares its own template. Run once per theme, rather than
    // repeating identical journeys for every unrelated screenshot fixture.
    // Traces land in `<output>/<theme>/journeys/usecases/<name>.jsonl`.
    let usecases_seed = options.usecases_seed.unwrap_or(0xC0DE_FEED_BEEF_C0DE);
    let usecases_count = options.usecases_count.unwrap_or(32);
    let mut total_usecases = 0usize;
    // Fixture clipboard content is declared independently of earlier native
    // paste probes, so generated journeys do not inherit unrelated input.
    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(String::new())));
    for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
        let (window, workspace) = open_fixture(&mut cx, "", theme)?;
        let usecase_result: Result<usize> = (|| {
            let n = crate::usecases::run_all(
                &mut cx,
                window,
                &workspace,
                usecases_seed,
                usecases_count,
                &options.output.join(theme_name(theme)).join("journeys"),
                options.record_frames,
            )?;
            Ok(n)
        })();
        match usecase_result {
            Ok(n) => total_usecases += n,
            Err(err) => {
                cx.update_window(window.into(), |_, window, _| window.remove_window())?;
                drop(workspace);
                cx.advance_clock(Duration::from_secs(2));
                cx.run_until_parked();
                return Err(err.context(format!("usecases ({}) failed", theme_name(theme))));
            }
        }
        cx.update_window(window.into(), |_, window, _| window.remove_window())?;
        drop(workspace);
        // Generated scenarios route every state change through
        // workspace-bound tasks. The drain mirrors the baseline loop:
        // drop the entity first so GPUI unregisters its handle, then
        // let the autosave / parse pumps quiesce before the next
        // fixture open — otherwise the auto-save task retains the
        // workspace's Document entity and the next iteration panics
        // with "Leaked handle for entity Document".
        cx.advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
    }
    println!("PASS usecases: {total_usecases} scenarios total");

    println!(
        "PASS: {snapshots} native GUI states; geometry{}{}",
        if checked_input {
            ", input, undo, and mode roundtrip"
        } else {
            ""
        },
        if options.geometry_only {
            " (screenshots explicitly disabled)"
        } else {
            "; Metal screenshots"
        }
    );
    println!("Artifacts: {}", options.output.display());
    Ok(())
}

fn theme_name(theme: ThemeChoice) -> &'static str {
    match theme {
        ThemeChoice::Dark => "dark",
        ThemeChoice::Light => "light",
    }
}

fn source_scroll_state(
    cx: &HeadlessAppContext,
    workspace: &Entity<Workspace>,
) -> (
    gpui::Bounds<gpui::Pixels>,
    gpui::Pixels,
    gpui::Point<gpui::Pixels>,
) {
    cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .unwrap()
            .editor_view
            .read(cx)
            .horizontal_scroll_state()
    })
}

fn scroll_source(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    delta_x: f32,
) -> Result<()> {
    let (viewport, _, _) = source_scroll_state(cx, workspace);
    cx.update_window(window.into(), |_, window, cx| {
        window.simulate_mouse_move(viewport.center(), cx);
        window.dispatch_event(
            gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position: viewport.center(),
                delta: gpui::ScrollDelta::Pixels(gpui::point(px(delta_x), px(0.))),
                modifiers: Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            }),
            cx,
        );
    })?;
    draw(cx, window)
}

fn check_source_horizontal_access(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    theme: ThemeChoice,
    options: &Options,
) -> Result<()> {
    let content = document_text(cx, workspace);
    let paragraph_start = content
        .find("Alexey's personal")
        .context("missing long-line fixture")?;
    let paragraph_end = paragraph_start + content[paragraph_start..].find('\n').unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.resize(size(px(960.), px(HEIGHT)));
        window.bounds_changed(cx);
    })?;
    for (shortcut, name) in [("alt-cmd-2", "source"), ("alt-cmd-3", "split")] {
        cx.update_window(window.into(), |_, window, cx| {
            window.resize(size(px(960.), px(HEIGHT)));
            window.bounds_changed(cx);
        })?;
        keystroke(cx, window, shortcut)?;
        cx.update_window(window.into(), |_, window, cx| {
            let editor = workspace.read(cx).active_tab().unwrap().editor.clone();
            window.focus(&editor.read(cx).focus_handle.clone(), cx);
            editor.update(cx, |editor, cx| editor.jump_to(paragraph_start, cx));
        })?;
        draw(cx, window)?;
        let initial = source_scroll_state(cx, workspace);
        let mut evidence = format!("initial: {initial:?}\n");
        let evidence_path = options
            .output
            .join(format!("horizontal-{}-{name}.txt", theme_name(theme)));
        std::fs::write(&evidence_path, &evidence)?;
        ensure!(
            f32::from(initial.1) > f32::from(initial.0.size.width) * 1.5,
            "{name}: long source line did not create a horizontal scroll extent"
        );
        ensure!(
            f32::from(initial.2.x).abs() < 1.,
            "{name}: paragraph Home did not reveal the start of the line"
        );

        scroll_source(cx, window, workspace, -100_000.)?;
        let scrolled = source_scroll_state(cx, workspace);
        evidence.push_str(&format!("scrolled right: {scrolled:?}\n"));
        std::fs::write(&evidence_path, &evidence)?;
        let rightmost = f32::from(scrolled.1 - scrolled.0.size.width);
        ensure!(rightmost > 100. && (f32::from(scrolled.2.x) + rightmost).abs() <= 1., "{name}: native horizontal wheel could not reach the right end of the physical line: {scrolled:?}");
        let source_color = cx.read_entity(workspace, |workspace, cx| {
            workspace
                .active_tab()
                .unwrap()
                .editor
                .read(cx)
                .theme
                .code_block_bg
        });
        // Oversized code-line backgrounds use the same native paint clip as
        // source glyphs. Assert the real scene mask, not just scroll metadata.
        let clips = cx.update_window(window.into(), |_, window, _| {
            let scale = window.scale_factor();
            let left = f32::from(scrolled.0.left()) * scale;
            let right = f32::from(scrolled.0.right()) * scale;
            let top = f32::from(scrolled.0.top()) * scale;
            let bottom = f32::from(scrolled.0.bottom()) * scale;
            let source_left = f32::from(scrolled.0.left() + scrolled.2.x) * scale;
            let source_width = f32::from(scrolled.1) * scale;
            let quads = window
                .painted_quads()
                .into_iter()
                .filter(|quad| {
                    quad.background == source_color.into()
                        && (quad.bounds.size.width.0 - source_width).abs() <= 1.
                        && (quad.bounds.left().0 - source_left).abs() <= 1.
                        && quad.bounds.top().0 < bottom
                        && quad.bounds.bottom().0 > top
                })
                .collect::<Vec<_>>();
            (quads, left, right)
        })?;
        ensure!(
            !clips.0.is_empty(),
            "{name}: no oversized source paint quad was observed to verify viewport clipping"
        );
        for quad in clips.0 {
            let left = quad.bounds.left().0.max(quad.content_mask.bounds.left().0);
            let right = quad
                .bounds
                .right()
                .0
                .min(quad.content_mask.bounds.right().0);
            ensure!(
                left >= clips.1 - 1. && right <= clips.2 + 1.,
                "{name}: horizontally scrolled source paint escaped the viewport into adjacent UI: visible {left}..{right}, viewport {}..{}, quad {quad:?}",
                clips.1,
                clips.2
            );
        }

        keystroke(cx, window, "cmd-right")?;
        let (viewport, _, offset) = source_scroll_state(cx, workspace);
        let caret = cx
            .read_entity(workspace, |workspace, cx| {
                workspace
                    .active_tab()
                    .unwrap()
                    .editor
                    .read(cx)
                    .painted_caret_bounds()
            })
            .context("source End did not paint a caret")?;
        let cursor = cx.read_entity(workspace, |workspace, cx| {
            workspace
                .active_tab()
                .unwrap()
                .editor
                .read(cx)
                .cursor_offset()
        });
        evidence.push_str(&format!(
            "End: offset={offset:?}, caret={caret:?}, source={cursor}\n"
        ));
        std::fs::write(&evidence_path, &evidence)?;
        ensure!(
            cursor == paragraph_end,
            "{name}: End did not reach the physical line end"
        );
        ensure!(
            caret.left() >= viewport.left() && caret.right() <= viewport.right(),
            "{name}: line-end caret remains clipped after horizontal navigation"
        );

        cx.update_window(window.into(), |_, window, cx| {
            window.resize(size(px(720.), px(HEIGHT)));
            window.bounds_changed(cx);
        })?;
        draw(cx, window)?;
        let resized = source_scroll_state(cx, workspace);
        let resized_caret = cx
            .read_entity(workspace, |workspace, cx| {
                workspace
                    .active_tab()
                    .unwrap()
                    .editor
                    .read(cx)
                    .painted_caret_bounds()
            })
            .context("resized source did not paint a caret")?;
        evidence.push_str(&format!(
            "resized at End: {resized:?}, caret={resized_caret:?}\n"
        ));
        std::fs::write(&evidence_path, &evidence)?;
        ensure!(
            resized.0.size.width < viewport.size.width
                && resized_caret.left() >= resized.0.left()
                && resized_caret.right() <= resized.0.right(),
            "{name}: shrinking the viewport lost the unchanged line-end caret"
        );

        keystroke(cx, window, "cmd-left")?;
        let home = source_scroll_state(cx, workspace);
        let caret = cx
            .read_entity(workspace, |workspace, cx| {
                workspace
                    .active_tab()
                    .unwrap()
                    .editor
                    .read(cx)
                    .painted_caret_bounds()
            })
            .context("source Home did not paint a caret")?;
        evidence.push_str(&format!("Home: {home:?}, caret={caret:?}\n"));
        std::fs::write(&evidence_path, &evidence)?;
        ensure!(
            f32::from(home.2.x).abs() < 1.
                && caret.left() >= home.0.left()
                && caret.right() <= home.0.right(),
            "{name}: Home did not return the caret and viewport to the line start"
        );
        ensure!(
            document_text(cx, workspace) == content,
            "{name}: horizontal navigation changed the document"
        );
        println!(
            "PASS horizontal-{}-{name} (native wheel, paint clip, End, Home)",
            theme_name(theme)
        );
    }
    keystroke(cx, window, "alt-cmd-1")?;
    cx.update_window(window.into(), |_, window, cx| {
        window.resize(size(px(1200.), px(HEIGHT)));
        window.bounds_changed(cx);
    })?;
    draw(cx, window)
}

/// A partial multi-line code selection must cover only its source-backed
/// glyphs, remain above the code background, and vanish after deselection.
fn check_source_selection_geometry(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    theme: ThemeChoice,
    options: &Options,
) -> Result<usize> {
    use crate::visual_contract::{
        painted_selection_rectangles, validate_selection_layering,
        validate_source_selection_geometry, Rect,
    };

    let content = document_text(cx, workspace);
    let start = content
        .find("\nCloudSkills/\n")
        .context("missing source code fixture")?
        + 3;
    let end = content
        .find("marketplace.json")
        .context("missing third code row")?
        + 6;
    cx.update_window(window.into(), |_, window, cx| {
        window.resize(size(px(720.), px(HEIGHT)));
        window.bounds_changed(cx);
    })?;
    for (shortcut, mode_name) in [("alt-cmd-2", "source"), ("alt-cmd-3", "split")] {
        keystroke(cx, window, shortcut)?;
        cx.update_window(window.into(), |_, window, cx| {
            let editor = workspace.read(cx).active_tab().unwrap().editor.clone();
            window.focus(&editor.read(cx).focus_handle.clone(), cx);
            editor.update(cx, |editor, cx| {
                editor.jump_to(start, cx);
                editor.select_to(end, cx);
            });
        })?;
        draw(cx, window)?;
        keystroke(cx, window, "shift-right")?;
        let expected_selection = start..end + 1;
        let check = |cx: &mut HeadlessAppContext,
                     suffix: &str,
                     expected: &std::ops::Range<usize>|
         -> Result<()> {
            let label = format!(
                "paragraph-{}-{mode_name}-selection-{suffix}",
                theme_name(theme)
            );
            let (selection, geometry, viewport, selection_color, code_color) =
                cx.read_entity(workspace, |workspace, cx| {
                    let tab = workspace.active_tab().unwrap();
                    let editor = tab.editor.read(cx);
                    let view = tab.editor_view.read(cx);
                    (
                        editor.selected_range.clone(),
                        view.painted_geometry(),
                        view.horizontal_scroll_state().0,
                        editor.theme.selection,
                        editor.theme.code_block_bg,
                    )
                });
            let viewport = Rect::from_bounds(viewport);
            let painted = cx.update_window(window.into(), |_, window, _| {
                painted_selection_rectangles(window, selection_color, viewport)
            })?;
            std::fs::write(options.output.join(format!("{label}.selection.txt")),
                format!("source selection: {selection:?}\nviewport: {viewport:?}\npainted: {painted:#?}\nsource rows: {geometry:#?}"))?;
            if !options.geometry_only {
                cx.capture_screenshot(window.into())
                    .context("source selection screenshot failed")?
                    .save(options.output.join(format!("{label}.png")))?;
            }
            ensure!(&selection == expected, "{label}: source keyboard selection differs from expected {expected:?}: {selection:?}");
            validate_source_selection_geometry(
                &geometry.rows,
                &selection,
                viewport,
                &painted,
                &label,
            )?;
            cx.update_window(window.into(), |_, window, _| {
                validate_selection_layering(window, selection_color, code_color, viewport, &label)
            })??;
            println!("PASS {label} (source-to-scene coverage and background layering)");
            Ok(())
        };
        check(cx, "partial", &expected_selection)?;
        keystroke(cx, window, "right")?;
        check(
            cx,
            "collapsed",
            &(expected_selection.end..expected_selection.end),
        )?;
        ensure!(
            document_text(cx, workspace) == content,
            "source selection altered document bytes"
        );
    }
    keystroke(cx, window, "alt-cmd-1")?;
    cx.update_window(window.into(), |_, window, cx| {
        window.resize(size(px(1200.), px(HEIGHT)));
        window.bounds_changed(cx);
    })?;
    draw(cx, window)?;
    Ok(4)
}

/// Exercises every Markdown editing toolbar button via its keyboard
/// shortcut and verifies the rich engine produced the expected change.
/// The toolbar is just an alternative dispatch path for the same
/// actions, so a green pass here means each toolbar button is wired to
/// the same EditorCommand the shortcut resolves to.
fn check_format_toolbar(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    markdown: &str,
    theme: ThemeChoice,
    options: &Options,
) -> Result<usize> {
    let mut snapshots = 0;
    // Make sure we start in WYSIWYG mode so block commands land on the
    // rich surface (the toolbar greys them out in Source, which is
    // covered by `paragraph-light-source` baseline).
    cx.update_window(window.into(), |_, window, cx| {
        window.resize(size(px(720.), px(HEIGHT)));
        window.bounds_changed(cx);
    })?;
    keystroke(cx, window, "alt-cmd-1")?;
    keystroke(cx, window, "cmd-home")?;

    // The earlier `check_input_selection_and_modes` test leaves the
    // undo stack in a complex state, so we just verify the apply side
    // and capture a screenshot. The other block commands have rich-
    // engine test coverage in markrust-core; this scenario focuses on
    // the end-to-end toolbar wiring for the most-used block command.
    let _ = markdown; // keep the signature symmetric with the other checks

    // Cmd-1 converts the first paragraph to H1.
    keystroke(cx, window, "cmd-1")?;
    let after_h1 = document_text(cx, workspace);
    ensure!(
        after_h1.starts_with("# CloudSkills"),
        "Cmd-1 did not leave the document with an H1 at the start; got: {after_h1:?}"
    );
    snapshots += 1;
    capture(
        cx,
        window,
        workspace,
        &format!("paragraph-{}-h1", theme_name(theme)),
        options,
        false,
    )?;

    println!(
        "PASS format-toolbar-{theme} (Cmd-1 H1 roundtrip + screenshot)",
        theme = theme_name(theme)
    );
    Ok(snapshots)
}

fn check_responsive_shell(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    theme: ThemeChoice,
    options: &Options,
) -> Result<usize> {
    workspace.update(cx, |workspace, cx| {
        workspace.sidebar_open = true;
        workspace.outline_open = true;
        cx.notify();
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.resize(size(px(720.), px(HEIGHT)));
        window.bounds_changed(cx);
    })?;
    keystroke(cx, window, "cmd-up")?;
    assert_panels(
        cx,
        workspace,
        true,
        false,
        "narrow window keeps a usable document by collapsing the outline",
    )?;
    capture(
        cx,
        window,
        workspace,
        &format!("shell-{}-720", theme_name(theme)),
        options,
        true,
    )?;

    keystroke(cx, window, "ctrl-cmd-o")?;
    assert_panels(
        cx,
        workspace,
        false,
        true,
        "opening the outline must replace the narrow sidebar",
    )?;
    capture(
        cx,
        window,
        workspace,
        &format!("shell-{}-outline-720", theme_name(theme)),
        options,
        true,
    )?;
    keystroke(cx, window, "ctrl-cmd-s")?;
    assert_panels(
        cx,
        workspace,
        true,
        false,
        "opening the sidebar must replace the narrow outline",
    )?;
    keystroke(cx, window, "ctrl-cmd-s")?;
    assert_panels(
        cx,
        workspace,
        false,
        false,
        "the sidebar keyboard toggle must close it",
    )?;

    keystroke(cx, window, "alt-cmd-3")?;
    ensure!(
        cx.read_entity(workspace, |workspace, cx| workspace
            .active_tab()
            .unwrap()
            .editor
            .read(cx)
            .raw_source()),
        "Split left pane must display literal Markdown source"
    );
    assert_panels(
        cx,
        workspace,
        false,
        false,
        "narrow split mode must preserve both editing panes",
    )?;
    capture(
        cx,
        window,
        workspace,
        &format!("shell-{}-split-720", theme_name(theme)),
        options,
        true,
    )?;
    let split_width = cx.read_entity(workspace, |workspace, cx| {
        f32::from(
            workspace
                .active_tab()
                .unwrap()
                .rich_view
                .read(cx)
                .painted_viewport_bounds()
                .unwrap()
                .size
                .width,
        )
    });
    let layout_heading_offset = cx
        .read_entity(workspace, |workspace, cx| {
            let document = workspace.active_tab().unwrap().document.read(cx);
            let content = document.buffer.content();
            outline_headings(&document.syntax_spans, &content)
                .into_iter()
                .find(|(_, _, title)| title == "Layout")
                .map(|(offset, _, _)| offset)
        })
        .context("paragraph fixture has no Layout heading")?;
    let layout_text_offset = layout_heading_offset + "## ".len();
    for (shortcut, sidebar, outline, label) in [
        ("ctrl-cmd-o", false, true, "split-outline"),
        ("ctrl-cmd-s", true, false, "split-sidebar"),
    ] {
        keystroke(cx, window, shortcut)?;
        assert_panels(
            cx,
            workspace,
            sidebar,
            outline,
            "compact split panels must switch overlays",
        )?;
        let width = cx.read_entity(workspace, |workspace, cx| {
            f32::from(
                workspace
                    .active_tab()
                    .unwrap()
                    .rich_view
                    .read(cx)
                    .painted_viewport_bounds()
                    .unwrap()
                    .size
                    .width,
            )
        });
        ensure!(
            (width - split_width).abs() < 1.,
            "{label}: overlay shrank the split document from {split_width}px to {width}px"
        );
        capture(
            cx,
            window,
            workspace,
            &format!("shell-{}-{label}-720", theme_name(theme)),
            options,
            true,
        )?;
        if outline {
            ensure!(
                cx.read_entity(workspace, |workspace, _| workspace.panel_overlay
                    == Some(Panel::Outline)),
                "compact Split outline must be floating"
            );
            click_outline_layout_row(cx, window, 720.)?;
            let actual = cx.update_window(window.into(), |_, window, cx| {
                let workspace = workspace.read(cx);
                let tab = workspace.active_tab().unwrap();
                (
                    workspace.outline_open,
                    workspace.panel_overlay,
                    tab.rich_view.read(cx).is_focused(window),
                    tab.rich_view.read(cx).selected_range.start,
                    tab.editor.read(cx).selected_range.start,
                )
            })?;
            ensure!(
                actual == (false, None, true, layout_text_offset, layout_heading_offset),
                "selecting a floating Outline heading did not dismiss it, focus the editor, and navigate both panes: got {actual:?}, heading byte {layout_heading_offset}"
            );
        }
    }
    keystroke(cx, window, "ctrl-cmd-s")?;
    assert_panels(
        cx,
        workspace,
        false,
        false,
        "the compact overlay must close on a repeated toggle",
    )?;
    cx.update_window(window.into(), |_, window, cx| {
        window.resize(size(px(1200.), px(HEIGHT)));
        window.bounds_changed(cx);
    })?;
    draw(cx, window)?;
    keystroke(cx, window, "ctrl-cmd-o")?;
    ensure!(
        cx.read_entity(workspace, |workspace, _| {
            workspace.outline_open && workspace.panel_overlay.is_none()
        }),
        "wide Split outline should be docked"
    );
    click_outline_layout_row(cx, window, 1200.)?;
    ensure!(
        cx.update_window(window.into(), |_, window, cx| {
            let workspace = workspace.read(cx);
            let tab = workspace.active_tab().unwrap();
            workspace.outline_open
                && workspace.panel_overlay.is_none()
                && tab.rich_view.read(cx).is_focused(window)
                && tab.rich_view.read(cx).selected_range.start == layout_text_offset
        })?,
        "selecting a docked Outline heading must keep the panel open and focus the editor"
    );
    keystroke(cx, window, "ctrl-cmd-o")?;
    Ok(5)
}

fn click_outline_layout_row(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    width: f32,
) -> Result<()> {
    // The second heading sits below the fixed 46/36/36px toolbar rows and
    // Outline header. Keep the click inside the 220px panel at either width.
    let position = point(px(width - OUTLINE_WIDTH + 48.), px(196.));
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

fn assert_panels(
    cx: &HeadlessAppContext,
    workspace: &Entity<Workspace>,
    sidebar: bool,
    outline: bool,
    reason: &str,
) -> Result<()> {
    let actual = cx.read_entity(workspace, |workspace, _| {
        (workspace.sidebar_open, workspace.outline_open)
    });
    ensure!(
        actual == (sidebar, outline),
        "{reason}: expected panels {:?}, got {actual:?}",
        (sidebar, outline)
    );
    Ok(())
}

fn check_tab_navigation_and_markup_hints(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    theme: ThemeChoice,
    options: &Options,
) -> Result<()> {
    let (first_id, first_selection, first_content) = cx.read_entity(workspace, |workspace, cx| {
        let tab = workspace.active_tab().unwrap();
        (
            tab.id,
            tab.rich_view.read(cx).selected_range.clone(),
            tab.document.read(cx).buffer.content(),
        )
    });
    cx.update_window(window.into(), |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.new_document(window, cx));
    })?;
    keystroke(cx, window, "alt-cmd-2")?;
    ensure!(
        cx.update_window(window.into(), |_, window, cx| {
            let workspace = workspace.read(cx);
            let tab = workspace.active_tab().unwrap();
            tab.mode == EditorMode::Source
                && tab.editor.read(cx).focus_handle(cx).is_focused(window)
                && !tab.editor.read(cx).raw_source()
        })?,
        "new tab did not enter masked Source mode with editor focus"
    );

    // The first tab's label occupies the left side of the 36px tab strip.
    // Click inside its text, below the 46px document and 36px formatting rows.
    let position = point(px(40.), px(46. + 36. + 18.));
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
    draw(cx, window)?;
    ensure!(
        cx.update_window(window.into(), |_, window, cx| {
            let workspace = workspace.read(cx);
            let tab = workspace.active_tab().unwrap();
            tab.id == first_id
                && tab.mode == EditorMode::Wysiwyg
                && tab.rich_view.read(cx).is_focused(window)
                && tab.rich_view.read(cx).selected_range == first_selection
                && tab.document.read(cx).buffer.content() == first_content
        })?,
        "clicking a tab did not restore that editor's focus and selection"
    );
    for shortcut in ["ctrl-cmd-o", "ctrl-cmd-s"] {
        keystroke(cx, window, shortcut)?;
        ensure!(
            cx.update_window(window.into(), |_, window, cx| {
                let workspace = workspace.read(cx);
                let tab = workspace.active_tab().unwrap();
                tab.id == first_id
                    && tab.rich_view.read(cx).is_focused(window)
                    && tab.rich_view.read(cx).selected_range == first_selection
            })?,
            "{shortcut} changed the focused tab or selection"
        );
    }
    keystroke(cx, window, "alt-cmd-4")?;
    ensure!(
        cx.read_entity(workspace, |workspace, cx| {
            !workspace.config.markup_hints_enabled
                && workspace
                    .tabs
                    .iter()
                    .all(|tab| !tab.rich_view.read(cx).markup_hints_enabled())
        }),
        "Show Markup Hints did not update every open tab"
    );
    let before_edit = document_text(cx, workspace);
    let bold_caret = before_edit
        .find("bold text")
        .context("paragraph fixture has no bold span")?
        + 2;
    workspace.update(cx, |workspace, cx| {
        workspace
            .active_tab()
            .unwrap()
            .rich_view
            .update(cx, |view, cx| view.jump_to(bold_caret, cx));
    });
    draw(cx, window)?;
    let (selection, caret, viewport) = cx.read_entity(workspace, |workspace, cx| {
        let view = workspace.active_tab().unwrap().rich_view.read(cx);
        let (_, viewport, caret) = view.test_viewport_state();
        (view.selected_range.clone(), caret, viewport)
    });
    let caret = caret.context("markup hints off hid the caret inside bold text")?;
    ensure!(
        selection.start == bold_caret
            && selection.end == bold_caret
            && caret.left() >= viewport.left()
            && caret.right() <= viewport.right()
            && caret.top() >= viewport.top()
            && caret.bottom() <= viewport.bottom(),
        "markup hints off misplaced the bold-span caret: selection {selection:?}, caret {caret:?}, viewport {viewport:?}"
    );
    capture(
        cx,
        window,
        workspace,
        &format!(
            "paragraph-{}-markup-hints-off-bold-caret",
            theme_name(theme)
        ),
        options,
        true,
    )?;
    keystroke(cx, window, "x")?;
    ensure!(
        document_text(cx, workspace).contains("boxld text"),
        "markup hints off prevented editing inside bold text"
    );
    keystroke(cx, window, "cmd-z")?;
    ensure!(
        document_text(cx, workspace) == before_edit,
        "Undo did not restore the bold text after editing with markup hints off"
    );
    cx.update_window(window.into(), |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.new_document(window, cx));
    })?;
    let last_id = cx.read_entity(workspace, |workspace, cx| {
        let tab = workspace.active_tab().unwrap();
        assert!(!tab.rich_view.read(cx).markup_hints_enabled());
        tab.id
    });
    cx.update_window(window.into(), |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.close_tab(0, window, cx));
    })?;
    ensure!(
        cx.update_window(window.into(), |_, window, cx| {
            let workspace = workspace.read(cx);
            let tab = workspace.active_tab().unwrap();
            tab.id == last_id && tab.rich_view.read(cx).is_focused(window)
        })?,
        "closing an earlier tab changed the active document"
    );
    keystroke(cx, window, "alt-cmd-4")?;
    ensure!(
        cx.read_entity(workspace, |workspace, cx| {
            workspace.config.markup_hints_enabled
                && workspace
                    .tabs
                    .iter()
                    .all(|tab| tab.rich_view.read(cx).markup_hints_enabled())
        }),
        "Show Markup Hints did not restore the preference"
    );
    cx.update_window(window.into(), |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.close_tab(0, window, cx);
            workspace.close_tab(0, window, cx);
        });
    })?;
    ensure!(
        cx.read_entity(workspace, |workspace, _| {
            workspace.tabs.len() == 1 && workspace.active_tab().unwrap().id == last_id
        }),
        "closing the last tab must leave its document open"
    );
    println!("PASS tab-click-focus-and-markup-hints");
    Ok(())
}

fn open_fixture(
    cx: &mut HeadlessAppContext,
    markdown: &str,
    theme: ThemeChoice,
) -> Result<(WindowHandle<MarkRustWindow>, Entity<Workspace>)> {
    let mut workspace_handle = None;
    let window = cx.open_window(size(px(WIDTHS[0]), px(HEIGHT)), |window, cx| {
        let workspace = cx.new(|cx| {
            let mut workspace = Workspace::new_for_gui_tests(
                AppConfig {
                    theme,
                    ..AppConfig::default()
                },
                window,
                cx,
            );
            workspace.sidebar_open = false;
            workspace.outline_open = false;
            let tab = workspace.active_tab().unwrap();
            tab.document.update(cx, |document, cx| {
                document.replace_content(markdown);
                assert!(
                    document.wait_for_parse(Duration::from_secs(5)),
                    "fixture parse timed out"
                );
                cx.notify();
            });
            window.focus(&tab.rich_view.read(cx).focus_handle(cx), cx);
            workspace
        });
        workspace_handle = Some(workspace.clone());
        cx.new(|cx| MarkRustWindow::new(workspace, cx))
    })?;
    Ok((
        window,
        workspace_handle.context("fixture workspace was not created")?,
    ))
}

fn draw(cx: &mut HeadlessAppContext, window: WindowHandle<MarkRustWindow>) -> Result<()> {
    // A second pass resolves virtualized list item measurements and next-frame
    // invalidations. No wall-clock sleeps or cursor-blink advancement.
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

fn geometry(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> Vec<PaintedLeafGeometry> {
    cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .unwrap()
            .rich_view
            .read(cx)
            .painted_geometry()
    })
}

fn validate_geometry(leaves: &[PaintedLeafGeometry], label: &str) -> Result<()> {
    crate::visual_contract::validate_text_geometry(leaves, label)
}

fn capture(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    label: &str,
    options: &Options,
    rich_visible: bool,
) -> Result<()> {
    let leaves = geometry(cx, workspace);
    // Write diagnostics before asserting so failures always have useful artifacts.
    std::fs::write(
        options.output.join(format!("{label}.geometry.txt")),
        format!("{leaves:#?}"),
    )?;
    let screenshot = if !options.geometry_only {
        let screenshot = cx.capture_screenshot(window.into()).context("Metal capture failed; run on a Mac with a GPU, or explicitly use --geometry-only for layout/input checks")?;
        screenshot.save(options.output.join(format!("{label}.png")))?;
        let colors = screenshot
            .pixels()
            .map(|pixel| pixel.0)
            .collect::<std::collections::HashSet<_>>();
        ensure!(
            colors.len() > 100,
            "{label}: screenshot is unexpectedly blank or missing antialiased native glyphs"
        );
        Some(screenshot)
    } else {
        None
    };
    if rich_visible {
        validate_geometry(&leaves, label)?;
        if label.ends_with("-720") || label.ends_with("-1200") {
            let fixture = if label.starts_with("paragraph-") {
                Some(("End of paragraph fixture.", 6))
            } else if label.starts_with("lists-") {
                Some(("End of list fixture.", 9))
            } else if label.starts_with("table-") {
                Some(("End of table fixture.", 16))
            } else {
                None
            };
            if let Some((end_marker, minimum_leaves)) = fixture {
                ensure!(
                    leaves.iter().any(|leaf| leaf.text.contains(end_marker)),
                    "{label}: fixture bottom is missing from the painted viewport"
                );
                ensure!(
                    leaves.len() >= minimum_leaves,
                    "{label}: expected content leaves are missing"
                );
            }
        }
        let viewport_width = cx.update_window(window.into(), |_, window, _| {
            f32::from(window.viewport_size().width)
        })?;
        let pane = cx
            .read_entity(workspace, |workspace, cx| {
                workspace
                    .active_tab()
                    .unwrap()
                    .rich_view
                    .read(cx)
                    .painted_viewport_bounds()
            })
            .context("rich editor did not report its native pane bounds")?;
        let mode = cx.read_entity(workspace, |workspace, _| {
            workspace.active_tab().unwrap().mode
        });
        let minimum_document_width = if mode == EditorMode::Split {
            300.
        } else {
            400.
        };
        ensure!(f32::from(pane.size.width) >= minimum_document_width,
            "{label}: document pane is only {:.1}px wide; {:?} needs at least {minimum_document_width}px at supported window sizes",
            f32::from(pane.size.width), mode);
        for leaf in &leaves {
            for row in &leaf.lines {
                ensure!(
                    row.left >= -1. && row.right <= viewport_width + 1.,
                    "{label}: painted glyphs escape viewport width {viewport_width}: {row:?}"
                );
                ensure!(
                    row.left >= f32::from(pane.left()) - 1.
                        && row.right <= f32::from(pane.right()) + 1.,
                    "{label}: painted glyphs escape editor pane {pane:?}: {row:?}"
                );
            }
        }
    }
    // Never approve a golden image whose geometry assertions have failed.
    if let Some(screenshot) = screenshot {
        if let Some(baseline) = &options.baseline {
            let path = baseline.join(format!("{label}.png"));
            if options.update_baselines {
                screenshot.save(path)?;
            } else {
                compare_baseline(
                    &screenshot,
                    &path,
                    &options.output.join(format!("{label}.diff.png")),
                )?;
            }
        }
    }
    println!("PASS {label}");
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

fn document_text(cx: &HeadlessAppContext, workspace: &Entity<Workspace>) -> String {
    cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .unwrap()
            .document
            .read(cx)
            .buffer
            .content()
    })
}

fn check_input_selection_and_modes(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    workspace: &Entity<Workspace>,
    markdown: &str,
    theme: ThemeChoice,
    options: &Options,
) -> Result<()> {
    keystroke(cx, window, "cmd-down")?;
    let insertion = cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .unwrap()
            .rich_view
            .read(cx)
            .cursor_offset()
    });
    ensure!(
        insertion >= markdown.trim_end_matches('\n').len(),
        "Cmd-Down did not reach the final document position"
    );
    let typed = " Native 👩🏽‍💻";
    let expected = format!(
        "{}{typed}{}",
        &markdown[..insertion],
        &markdown[insertion..]
    );
    for character in typed.chars() {
        cx.update_window(window.into(), |_, window, cx| {
            let key = character.to_string();
            window.dispatch_keystroke(
                Keystroke {
                    modifiers: Modifiers::default(),
                    key: key.clone(),
                    key_char: Some(key),
                },
                cx,
            );
        })?;
        draw(cx, window)?;
    }
    draw(cx, window)?;
    ensure!(
        document_text(cx, workspace) == expected,
        "native text input did not reach the editor at document end: {:?}",
        document_text(cx, workspace)
    );
    keystroke(cx, window, "backspace")?;
    ensure!(
        document_text(cx, workspace)
            == format!(
                "{} Native {}",
                &markdown[..insertion],
                &markdown[insertion..]
            ),
        "native Backspace split an emoji grapheme"
    );
    keystroke(cx, window, "cmd-z")?;
    ensure!(
        document_text(cx, workspace) == expected,
        "native Undo did not restore the deleted grapheme"
    );

    // Exercise selection through the same keyboard dispatch used by the app.
    keystroke(cx, window, "cmd-a")?;
    let selection = cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .unwrap()
            .rich_view
            .read(cx)
            .selected_range
            .clone()
    });
    ensure!(
        selection == (0..document_text(cx, workspace).len()),
        "Cmd-A failed to select all document text"
    );
    let label = format!("paragraph-{}-selection", theme_name(theme));
    let check_painted_selection = |cx: &mut HeadlessAppContext, label: &str| -> Result<()> {
        use crate::visual_contract::{validate_selection_geometry, Rect};

        let leaves = geometry(cx, workspace);
        let (selection, viewport, color) = cx.read_entity(workspace, |workspace, cx| {
            let view = workspace.active_tab().unwrap().rich_view.read(cx);
            (
                view.selected_range.clone(),
                view.painted_viewport_bounds(),
                view.theme.selection,
            )
        });
        let viewport = Rect::from_bounds(viewport.context("selection viewport was not painted")?);
        let painted = cx.update_window(window.into(), |_, window, _| {
            let scale = window.scale_factor();
            window
                .painted_quads()
                .into_iter()
                .filter(|quad| quad.background == color.into())
                .filter_map(|quad| {
                    // Verify what the scene actually exposes after clipping,
                    // not the selection painter's own intermediate geometry.
                    let rect = Rect {
                        left: quad.bounds.left().0.max(quad.content_mask.bounds.left().0) / scale,
                        top: quad.bounds.top().0.max(quad.content_mask.bounds.top().0) / scale,
                        right: quad
                            .bounds
                            .right()
                            .0
                            .min(quad.content_mask.bounds.right().0)
                            / scale,
                        bottom: quad
                            .bounds
                            .bottom()
                            .0
                            .min(quad.content_mask.bounds.bottom().0)
                            / scale,
                    };
                    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
                })
                .collect::<Vec<_>>()
        })?;
        std::fs::write(options.output.join(format!("{label}.selection.txt")),
            format!("source selection: {selection:?}\nviewport: {viewport:?}\npainted: {painted:#?}\nleaves: {leaves:#?}"))?;
        validate_selection_geometry(&leaves, &selection, viewport, &painted, label)
    };
    check_painted_selection(cx, &label)?;
    capture(cx, window, workspace, &label, options, true)?;
    keystroke(cx, window, "right")?;
    check_painted_selection(cx, &format!("{label}-collapsed"))?;
    keystroke(cx, window, "shift-left")?;
    keystroke(cx, window, "shift-left")?;
    check_painted_selection(cx, &format!("{label}-partial"))?;
    let before_selection = cx.read_entity(workspace, |workspace, cx| {
        let view = workspace.active_tab().unwrap().rich_view.read(cx);
        (view.selected_range.clone(), view.selection_reversed)
    });
    let content_before_modes = document_text(cx, workspace);
    for (mode, name, shortcut) in [
        (EditorMode::Source, "source", "alt-cmd-2"),
        (EditorMode::Split, "split", "alt-cmd-3"),
        (EditorMode::Wysiwyg, "wysiwyg", "alt-cmd-1"),
    ] {
        keystroke(cx, window, shortcut)?;
        ensure!(
            cx.read_entity(workspace, |workspace, _| workspace
                .active_tab()
                .unwrap()
                .mode)
                == mode,
            "mode shortcut failed to switch to {name}"
        );
        ensure!(
            document_text(cx, workspace) == content_before_modes,
            "switching to {name} changed the document"
        );
        let after_selection = cx.read_entity(workspace, |workspace, cx| {
            let tab = workspace.active_tab().unwrap();
            if mode == EditorMode::Source {
                let editor = tab.editor.read(cx);
                (editor.selected_range.clone(), editor.selection_reversed)
            } else {
                let editor = tab.rich_view.read(cx);
                (editor.selected_range.clone(), editor.selection_reversed)
            }
        });
        ensure!(
            after_selection == before_selection,
            "switching to {name} lost the selection or active end"
        );
        if mode != EditorMode::Wysiwyg {
            capture(
                cx,
                window,
                workspace,
                &format!("paragraph-{}-{name}", theme_name(theme)),
                options,
                mode == EditorMode::Split,
            )?;
        }
    }
    // HeadlessAppContext uses its own TestPlatform clipboard, never the user's
    // system clipboard. Exercise the Edit menu's actual Paste/Undo/Redo route.
    let pasted = "Café 👩🏽‍💻";
    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(pasted.into())));
    let expected_paste = format!(
        "{}{pasted}{}",
        &content_before_modes[..before_selection.0.start],
        &content_before_modes[before_selection.0.end..]
    );
    keystroke(cx, window, "cmd-v")?;
    ensure!(
        document_text(cx, workspace) == expected_paste,
        "native Paste did not replace the selection with Unicode clipboard text"
    );
    keystroke(cx, window, "cmd-z")?;
    ensure!(
        document_text(cx, workspace) == content_before_modes,
        "Undo did not restore text after Paste: {:?}",
        document_text(cx, workspace)
    );
    keystroke(cx, window, "cmd-shift-z")?;
    ensure!(
        document_text(cx, workspace) == expected_paste,
        "Redo did not restore the pasted text"
    );
    keystroke(cx, window, "cmd-z")?;
    keystroke(cx, window, "right")?;
    let insertion = cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .unwrap()
            .rich_view
            .read(cx)
            .cursor_offset()
    });
    let expected_paste = format!(
        "{}{pasted}{}",
        &content_before_modes[..insertion],
        &content_before_modes[insertion..]
    );
    keystroke(cx, window, "cmd-v")?;
    ensure!(
        document_text(cx, workspace) == expected_paste,
        "Paste at a collapsed caret inserted incorrect text"
    );
    keystroke(cx, window, "cmd-z")?;
    ensure!(
        document_text(cx, workspace) == content_before_modes,
        "Undo coalesced an explicit Paste with previous typing"
    );
    Ok(())
}

fn compare_baseline(actual: &RgbaImage, path: &Path, diff_path: &Path) -> Result<()> {
    ensure!(path.is_file(), "Missing approved baseline {}. Review artifacts and explicitly use --update-baselines to approve them.", path.display());
    let expected = image::open(path)?.into_rgba8();
    ensure!(
        actual.dimensions() == expected.dimensions(),
        "Screenshot dimensions changed for {}: {:?} != {:?}",
        path.display(),
        actual.dimensions(),
        expected.dimensions()
    );
    let mut changed = 0usize;
    let mut difference = RgbaImage::new(actual.width(), actual.height());
    for ((actual_pixel, expected_pixel), diff) in actual
        .pixels()
        .zip(expected.pixels())
        .zip(difference.pixels_mut())
    {
        // Small channel differences permit native glyph antialiasing variance;
        // shifted rows, clipping, missing glyphs and changed controls still fail.
        let delta = actual_pixel
            .0
            .iter()
            .zip(expected_pixel.0)
            .map(|(a, b)| a.abs_diff(b))
            .max()
            .unwrap();
        if delta > 12 {
            changed += 1;
            *diff = Rgba([255, 0, 100, 255]);
        } else {
            *diff = Rgba([
                actual_pixel[0] / 4,
                actual_pixel[1] / 4,
                actual_pixel[2] / 4,
                255,
            ]);
        }
    }
    let ratio = changed as f64 / (actual.width() as f64 * actual.height() as f64);
    if ratio > 0.001 {
        difference.save(diff_path)?;
        bail!(
            "Visual regression in {}: {:.3}% of pixels differ (limit 0.1%); diff {}",
            path.display(),
            ratio * 100.,
            diff_path.display()
        );
    }
    Ok(())
}

/// Exercise application-level key routing without quitting or hiding a real
/// process. These probes use the same App::on_action registration as menus.rs;
/// only the OS side effects are replaced by counters on the isolated test app.
fn check_application_command_routing(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
) -> Result<()> {
    use std::{cell::RefCell, rc::Rc};

    let routed = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        let quit = routed.clone();
        cx.on_action(move |_: &crate::menus::Quit, _| quit.borrow_mut().push("quit"));
        let hide = routed.clone();
        cx.on_action(move |_: &crate::menus::Hide, _| hide.borrow_mut().push("hide"));
        let hide_others = routed.clone();
        cx.on_action(move |_: &crate::menus::HideOthers, _| {
            hide_others.borrow_mut().push("hide-others")
        });
    });
    draw(cx, window)?;
    let focused = cx.update_window(window.into(), |_, window, cx| window.focused(cx))?;
    ensure!(
        focused.is_some(),
        "application command probe has no editor focus"
    );
    for context in ["editor-focused", "blurred"] {
        if context == "blurred" {
            cx.update_window(window.into(), |_, window, cx| window.blur(cx))?;
            draw(cx, window)?;
        }
        for (key, expected) in [
            ("cmd-q", "quit"),
            ("cmd-h", "hide"),
            ("alt-cmd-h", "hide-others"),
        ] {
            keystroke(cx, window, key)?;
            ensure!(
                *routed.borrow() == vec![expected],
                "{key} ({context}) did not reach its global application action exactly once: {:?}",
                routed.borrow()
            );
            routed.borrow_mut().clear();
        }
    }
    cx.update_window(window.into(), |_, window, cx| {
        window.focus(focused.as_ref().unwrap(), cx)
    })?;
    draw(cx, window)?;
    println!("PASS application-command-routing (editor-focused and blurred)");
    Ok(())
}
