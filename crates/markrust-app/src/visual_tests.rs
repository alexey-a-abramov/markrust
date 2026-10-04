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
use image::{ImageEncoder, Rgba, RgbaImage};
use markrust_editor::outline_headings;
use markrust_editor::wysiwyg::{PaintedLeafGeometry, RichEditorView};

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

#[derive(Clone)]
struct Options {
    output: PathBuf,
    baseline: Option<PathBuf>,
    update_baselines: bool,
    geometry_only: bool,
    filter: Option<String>,
    usecases_seed: Option<u64>,
    usecases_count: Option<usize>,
    record_frames: bool,
    concurrent_only: bool,
    notepad_only: bool,
    images_only: bool,
    open_path_only: bool,
    locale_only: bool,
    journeys: Option<String>,
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
            concurrent_only: false,
            notepad_only: false,
            images_only: false,
            open_path_only: false,
            locale_only: false,
            journeys: None,
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--output" => options.output = args.next().context("--output needs a path")?.into(),
                "--baseline" => {
                    options.baseline = Some(args.next().context("--baseline needs a path")?.into())
                }
                "--filter" => {
                    options.filter = Some(args.next().context("--filter needs a fixture name")?)
                }
                "--update-baselines" => options.update_baselines = true,
                "--geometry-only" => options.geometry_only = true,
                "--record-frames" => options.record_frames = true,
                "--concurrent-only" => options.concurrent_only = true,
                "--notepad-only" => options.notepad_only = true,
                "--images-only" => options.images_only = true,
                "--open-path-only" => options.open_path_only = true,
                "--locale-only" => options.locale_only = true,
                "--journeys" => options.journeys = Some(args.next().context("--journeys needs a scenario-name substring")?),
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
                _ => bail!(
                    "Unknown argument {arg}. Use --output PATH, --baseline PATH, --update-baselines, --geometry-only, --record-frames, --concurrent-only, --notepad-only, --images-only, --open-path-only, --locale-only, --journeys NAME, --filter NAME, --usecases-seed N, or --usecases-count N."
                ),
            }
        }
        ensure!(
            options.journeys.is_none()
                || (options.baseline.is_none()
                    && options.filter.is_none()
                    && !options.concurrent_only
                    && !options.notepad_only
                    && !options.images_only),
            "--journeys is an isolated curated subset, not a baseline or lifecycle run"
        );
        ensure!(
            !(options.open_path_only || options.locale_only)
                || (options.baseline.is_none()
                    && options.journeys.is_none()
                    && options.filter.is_none()
                    && !options.record_frames
                    && !options.concurrent_only
                    && !options.notepad_only
                    && !options.images_only
                    && !(options.open_path_only && options.locale_only)),
            "--open-path-only and --locale-only are isolated diagnostic subsets; omit baselines, journey frames, fixture filters and other subsets"
        );
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
            !options.concurrent_only || (options.baseline.is_none() && !options.record_frames),
            "--concurrent-only writes diagnostic snapshots, not golden baselines or journey frames"
        );
        ensure!(
            !options.notepad_only || (options.baseline.is_none() && !options.record_frames && !options.concurrent_only),
            "--notepad-only is an isolated diagnostic subset; omit baselines, journey frames and --concurrent-only"
        );
        ensure!(
            !options.images_only || (options.baseline.is_none() && !options.record_frames && !options.concurrent_only && !options.notepad_only && options.filter.is_none()),
            "--images-only is an isolated diagnostic subset; omit baselines, journey frames, fixture filters and other isolated subsets"
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
    if let Some(filter) = &options.journeys {
        let scenarios: Vec<_> = crate::usecases::curated_scenarios()
            .into_iter()
            .filter(|scenario| scenario.name.contains(filter))
            .collect();
        ensure!(
            !scenarios.is_empty(),
            "no curated journey matches {filter:?}"
        );
        for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
            let (window, workspace) = open_fixture(&mut cx, "", theme)?;
            let result = scenarios.iter().try_for_each(|scenario| {
                crate::usecases::run_scenario(
                    &mut cx,
                    window,
                    &workspace,
                    scenario,
                    &options.output.join(theme_name(theme)).join("journeys"),
                    options.record_frames,
                )
                .map(|_| ())
            });
            cx.update_window(window.into(), |_, window, _| window.remove_window())?;
            drop(workspace);
            cx.advance_clock(Duration::from_secs(2));
            cx.run_until_parked();
            result?;
        }
        println!(
            "PASS isolated journeys: {} (both themes); not a complete GUI gate",
            scenarios.len() * 2
        );
        println!("Artifacts: {}", options.output.display());
        return Ok(());
    }
    if options.concurrent_only {
        crate::concurrent_visual_tests::check(&mut cx, &options.output, options.geometry_only)?;
        println!("Artifacts: {}", options.output.display());
        return Ok(());
    }
    if options.images_only {
        let mut snapshots = 0;
        for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
            snapshots += check_image_editor(&mut cx, theme, &options)?;
        }
        println!("PASS: {snapshots} isolated image inspector states");
        println!("Artifacts: {}", options.output.display());
        return Ok(());
    }
    if options.notepad_only {
        crate::notepad_visual_tests::check(&mut cx, &options.output, options.geometry_only)?;
        println!("Artifacts: {}", options.output.display());
        return Ok(());
    }
    if options.open_path_only || options.locale_only {
        let mut snapshots = 0;
        for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
            snapshots += if options.open_path_only {
                check_open_path(&mut cx, theme, &options)?
            } else {
                check_ui_language(&mut cx, theme, &options)?
            };
        }
        println!("PASS: {snapshots} isolated desktop states; not a complete GUI gate");
        println!("Artifacts: {}", options.output.display());
        return Ok(());
    }
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
    // New curated regressions must not silently crowd generated journeys out.
    let usecases_count = options
        .usecases_count
        .unwrap_or_else(|| crate::usecases::curated_scenarios().len() + 5);
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
    for theme in [ThemeChoice::Light, ThemeChoice::Dark] {
        snapshots += check_session_recovery(&mut cx, theme, &options)?;
        check_pending_widget_persistence(&mut cx, theme, &options)?;
        snapshots += check_image_editor(&mut cx, theme, &options)?;
        snapshots += check_open_path(&mut cx, theme, &options)?;
        snapshots += check_ui_language(&mut cx, theme, &options)?;
    }
    snapshots +=
        crate::concurrent_visual_tests::check(&mut cx, &options.output, options.geometry_only)?;
    snapshots +=
        crate::notepad_visual_tests::check(&mut cx, &options.output, options.geometry_only)?;
    snapshots += crate::update_visual_tests::run_update_ui_checks(
        &mut cx,
        &options.output,
        options.geometry_only,
    )?;

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

        // Literal Source has no code-card background. Exercise clipping with
        // an actual oversized source selection instead of obsolete chrome.
        keystroke(cx, window, "cmd-shift-right")?;
        scroll_source(cx, window, workspace, -100_000.)?;
        let scrolled = source_scroll_state(cx, workspace);
        evidence.push_str(&format!("scrolled right: {scrolled:?}\n"));
        std::fs::write(&evidence_path, &evidence)?;
        let rightmost = f32::from(scrolled.1 - scrolled.0.size.width);
        ensure!(
            rightmost > 100. && (f32::from(scrolled.2.x) + rightmost).abs() <= 1.,
            "{name}: native horizontal wheel could not reach the right end of the physical line: {scrolled:?}"
        );
        let source_color = cx.read_entity(workspace, |workspace, cx| {
            workspace
                .active_tab()
                .unwrap()
                .editor
                .read(cx)
                .theme
                .selection
        });
        // Selection and source glyphs share the native paint clip. Inspect the
        // actual oversized scene quad, not just scroll metadata.
        let clips = cx.update_window(window.into(), |_, window, _| {
            let scale = window.scale_factor();
            let left = f32::from(scrolled.0.left()) * scale;
            let right = f32::from(scrolled.0.right()) * scale;
            let top = f32::from(scrolled.0.top()) * scale;
            let bottom = f32::from(scrolled.0.bottom()) * scale;
            let quads = window
                .painted_quads()
                .into_iter()
                .filter(|quad| {
                    quad.background == source_color.into()
                        && quad.bounds.size.width.0 > (right - left) * 1.5
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
            std::fs::write(
                options.output.join(format!("{label}.selection.txt")),
                format!(
                    "source selection: {selection:?}\nviewport: {viewport:?}\npainted: {painted:#?}\nsource rows: {geometry:#?}"
                ),
            )?;
            if !options.geometry_only {
                save_screenshot(
                    &cx.capture_screenshot(window.into())
                        .context("source selection screenshot failed")?,
                    &options.output.join(format!("{label}.png")),
                )?;
            }
            ensure!(
                &selection == expected,
                "{label}: source keyboard selection differs from expected {expected:?}: {selection:?}"
            );
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

/// Click the actual style buttons and verify that their visible order matches
/// the Markdown they produce. Shortcut coverage cannot prove button wiring.
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
    keystroke(cx, window, "cmd-up")?;
    let caret = cx.read_entity(workspace, |workspace, cx| {
        workspace
            .active_tab()
            .unwrap()
            .rich_view
            .read(cx)
            .cursor_offset()
    });
    let first_text = markdown
        .find(|ch: char| ch != '#' && ch != ' ')
        .context("format toolbar fixture has no first heading text")?;
    ensure!(
        caret == 0 || caret == first_text,
        "Document Home did not position the rich caret at the first heading: {caret}"
    );

    let before_scroll = crate::observation::capture(cx, window, workspace)?;
    let (tools, offset, maximum) = cx.update_window(window.into(), |root, _, cx| {
        root.downcast::<MarkRustWindow>()
            .expect("fixture root")
            .read(cx)
            .test_format_toolbar_state()
    })?;
    ensure!(
        maximum.x > px(0.),
        "compact formatting toolbar has no overflow range"
    );
    let position = point(tools.right() + px(16.), tools.top() + px(18.));
    click_at(cx, window, position)?;
    let (_, scrolled, _) = cx.update_window(window.into(), |root, _, cx| {
        root.downcast::<MarkRustWindow>()
            .expect("fixture root")
            .read(cx)
            .test_format_toolbar_state()
    })?;
    ensure!(
        scrolled.x < offset.x,
        "compact formatting overflow button did not scroll"
    );
    let after_scroll = crate::observation::capture(cx, window, workspace)?;
    crate::observation::validate_stationary_rich_layout(&before_scroll, &after_scroll)?;
    ensure!(
        before_scroll.rich_pane.viewport == after_scroll.rich_pane.viewport,
        "formatting overflow changed the document viewport"
    );
    click_at(
        cx,
        window,
        point(tools.left() - px(16.), tools.top() + px(18.)),
    )?;

    let expected_title = markdown
        .lines()
        .next()
        .context("format toolbar fixture has no title")?
        .trim_start_matches('#')
        .trim_start();
    let before = document_text(cx, workspace);
    let untouched_tail = before
        .split_once('\n')
        .context("format toolbar fixture has no body")?
        .1
        .to_owned();
    let (tools, offset, _) = cx.update_window(window.into(), |root, _, cx| {
        root.downcast::<MarkRustWindow>()
            .expect("fixture root")
            .read(cx)
            .test_format_toolbar_state()
    })?;
    ensure!(
        offset.x == px(0.),
        "format toolbar did not return to its first tools"
    );
    // Four 34 px inline buttons, 4 px gaps, then the 17 px separator.
    // These are physical toolbar coordinates, not direct command dispatches.
    let first_style_center = tools.left() + px(4. * (34. + 4.) + 17. + 4. + 17.);
    let style_position = |index: usize| {
        point(
            first_style_center + px(index as f32 * 38.),
            tools.top() + px(18.),
        )
    };
    let assert_style = |cx: &HeadlessAppContext, expected: &str, label: &str| -> Result<()> {
        let actual = document_text(cx, workspace);
        let (first_line, tail) = actual
            .split_once('\n')
            .context("format toolbar removed the fixture body")?;
        ensure!(
            first_line == expected,
            "{label} button produced {first_line:?}, expected {expected:?}"
        );
        ensure!(
            tail == untouched_tail,
            "{label} button changed unrelated document content"
        );
        Ok(())
    };

    // First remove the fixture's original heading; each following heading
    // click must change the source, making a missing or swapped handler fail.
    click_at(cx, window, style_position(3))?;
    assert_style(cx, expected_title, "Body")?;
    for (index, level) in [1usize, 2, 3].into_iter().enumerate() {
        click_at(cx, window, style_position(index))?;
        assert_style(
            cx,
            &format!("{} {expected_title}", "#".repeat(level)),
            &format!("H{level}"),
        )?;
    }
    click_at(cx, window, style_position(3))?;
    assert_style(cx, expected_title, "Body after H3")?;
    click_at(cx, window, style_position(0))?;
    assert_style(cx, &format!("# {expected_title}"), "H1 after Body")?;
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
        "PASS format-toolbar-{theme} (native overflow and H1/H2/H3/Body clicks, stable viewport, unrelated text preserved + screenshot)",
        theme = theme_name(theme)
    );
    Ok(snapshots)
}

/// Inspector input uses private plain-text fields, not the document's body
/// input owner. Keep this fixture independent of the body-caret oracle.
fn check_image_editor(
    cx: &mut HeadlessAppContext,
    theme: ThemeChoice,
    options: &Options,
) -> Result<usize> {
    let directory = options.output.join(format!(
        "image-editor-{}-{}",
        theme_name(theme),
        std::process::id()
    ));
    std::fs::create_dir_all(&directory)?;
    let image_bytes = include_bytes!("../tests/fixtures/assets/icon/icon.png");
    std::fs::write(directory.join("image.png"), image_bytes)?;
    std::fs::write(directory.join("replacement.png"), image_bytes)?;
    let original = "# Image inspector\n\n![Original image](image.png)\n\nKeep this paragraph.\n";
    let path = directory.join("document.md");
    std::fs::write(&path, original)?;
    let initial_document = markrust_core::Document::from_file(path.clone())?;
    let (window, workspace) = open_fixture(cx, original, theme)?;
    let (rich, document) = cx.read_entity(&workspace, |workspace, _| {
        let tab = workspace.active_tab().unwrap();
        (tab.rich_view.clone(), tab.document.clone())
    });
    let source_view = cx.read_entity(&workspace, |workspace, _| {
        workspace.active_tab().unwrap().editor_view.clone()
    });
    // A synthetic file path gives relative image destinations the production
    // base-directory semantics without opening a real user file or watcher.
    document.update(cx, |document, cx| {
        *document = initial_document;
        cx.notify();
    });
    let diagnostic_options = Options {
        baseline: None,
        update_baselines: false,
        ..options.clone()
    };
    let state = |cx: &HeadlessAppContext| {
        cx.read_entity(&rich, |rich, cx| rich.test_image_editor_state(cx))
    };
    let open = |cx: &mut HeadlessAppContext| -> Result<()> {
        let point = cx
            .read_entity(&rich, |rich, _| rich.test_first_image_hit_point())
            .context("image inspector fixture has no painted native image hit target")?;
        click_at(cx, window, point)?;
        draw(cx, window)?;
        ensure!(state(cx).is_some(), "image inspector did not open");
        let observed = crate::observation::capture(cx, window, &workspace)?;
        let mode = cx.read_entity(&workspace, |workspace, _| {
            match workspace.active_tab().unwrap().mode {
                EditorMode::Split => "split",
                EditorMode::Source => "source",
                EditorMode::Wysiwyg => "wysiwyg",
            }
        });
        crate::observation::validate(&observed, &document_text(cx, &workspace), mode)?;
        ensure!(
            matches!(observed.input_owner, crate::observation::InputOwner::Widget(ref field) if field == "image-location"),
            "image inspector did not own native location input"
        );
        Ok(())
    };
    let mut trace = Vec::new();
    let result: Result<()> = (|| {
        draw(cx, window)?;
        open(cx)?;
        for _ in 0..12 {
            if state(cx).is_some_and(|(_, _, approved)| approved) {
                break;
            }
            draw(cx, window)?;
        }
        ensure!(
            state(cx) == Some(("image.png".into(), "Original image".into(), true)),
            "relative raster image did not resolve into an approved inspector preview: {:?}",
            state(cx)
        );
        let raster_pixels = wait_for_image_preview_raster(
            cx,
            window,
            &rich,
            &diagnostic_options,
            &format!("image-editor-{}-preview", theme_name(theme)),
        )?;
        trace.push(serde_json::json!({"phase":"preview-raster-observed", "orange_pixels_inside_preview":raster_pixels, "geometry_only":options.geometry_only}));
        capture(
            cx,
            window,
            &workspace,
            &format!("image-editor-{}-preview", theme_name(theme)),
            &diagnostic_options,
            false,
        )?;

        // URL focus belongs to the field. Ordinary typing must not alter the
        // body, and Escape must discard even a changed, invalid draft.
        keystroke(cx, window, "cmd-a")?;
        keystroke(cx, window, "x")?;
        ensure!(
            state(cx).is_some_and(|(url, _, _)| url == "x"),
            "native typing did not reach the inspector URL field"
        );
        ensure!(
            document_text(cx, &workspace) == original,
            "typing in the inspector changed the Markdown body"
        );
        keystroke(cx, window, "cmd-z")?;
        ensure!(
            state(cx).is_some_and(|(url, _, _)| url == "image.png")
                && document_text(cx, &workspace) == original,
            "Undo in the URL field changed the body or failed to restore the URL: {:?}",
            state(cx)
        );
        keystroke(cx, window, "cmd-shift-z")?;
        ensure!(
            state(cx).is_some_and(|(url, _, _)| url == "x")
                && document_text(cx, &workspace) == original,
            "Redo in the URL field changed the body or failed to restore its private edit: {:?}",
            state(cx)
        );
        keystroke(cx, window, "cmd-a")?;
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.paste("replacement.png", window, cx)
            });
        })?;
        draw(cx, window)?;
        ensure!(
            state(cx).is_some_and(|(url, _, _)| url == "replacement.png")
                && document_text(cx, &workspace) == original,
            "workspace Paste escaped the focused URL field or changed the body: {:?}",
            state(cx)
        );
        keystroke(cx, window, "escape")?;
        ensure!(
            state(cx).is_none(),
            "Escape did not close the image inspector"
        );
        ensure!(
            document_text(cx, &workspace) == original,
            "Cancel wrote the private image draft into the document"
        );
        trace.push(serde_json::json!({ "phase": "cancel", "source": document_text(cx, &workspace), "inspector": state(cx) }));

        open(cx)?;
        let tab_alt = "Recoverable 👩🏽‍💻 image";
        rich.update(cx, |rich, cx| {
            rich.test_set_image_fields(tab_alt, "replacement.png", cx)
        });
        draw(cx, window)?;
        keystroke(cx, window, "tab")?;
        keystroke(cx, window, "x")?;
        let before_tab = cx
            .read_entity(&rich, |rich, _| rich.recovery_widget_draft())
            .context("private image fields were not mirrored into a recovery draft")?;
        keystroke(cx, window, "cmd-t")?;
        ensure!(
            cx.read_entity(&workspace, |workspace, _| workspace.active_tab) == 1,
            "Cmd-T did not leave the image inspector's tab"
        );
        ensure!(
            cx.read_entity(&rich, |rich, _| rich.recovery_widget_draft()) == Some(before_tab),
            "switching tabs changed or discarded the private image draft"
        );
        keystroke(cx, window, "ctrl-shift-tab")?;
        ensure!(
            cx.read_entity(&workspace, |workspace, _| workspace.active_tab) == 0,
            "Previous Tab did not return to the image inspector"
        );
        keystroke(cx, window, "x")?;
        ensure!(
            state(cx).is_some_and(|(url, alt, _)| {
                url == "replacement.png" && alt == format!("{tab_alt}xx")
            }) && document_text(cx, &workspace) == original,
            "tab return lost the draft, changed the body, or failed to restore alternative-text input: {:?}",
            state(cx)
        );
        trace.push(serde_json::json!({ "phase": "tab-return", "source": document_text(cx, &workspace), "inspector": state(cx) }));
        keystroke(cx, window, "escape")?;

        open(cx)?;
        let invalid_url = "javascript:alert(1)\ninvalid draft";
        let recovery_alt = "Recovered 👩🏽‍💻 alt\nsecond line";
        rich.update(cx, |rich, cx| {
            rich.test_set_image_fields(recovery_alt, invalid_url, cx)
        });
        draw(cx, window)?;
        let recovery = cx
            .read_entity(&rich, |rich, _| rich.recovery_widget_draft())
            .context("invalid image fields were omitted from private recovery")?;
        ensure!(
            recovery.kind == markrust_editor::wysiwyg::WidgetDraftKind::ImageProperties
                && recovery.original_source == original,
            "image recovery lost its typed target or original-source anchor"
        );
        keystroke(cx, window, "escape")?;
        ensure!(
            cx.read_entity(&rich, |rich, _| rich.recovery_widget_draft())
                .is_none()
                && document_text(cx, &workspace) == original,
            "Cancel left an image recovery draft or changed the body"
        );
        rich.update(cx, |rich, cx| {
            rich.restore_recovery_widget_draft(&recovery, cx)
        })?;
        draw(cx, window)?;
        ensure!(
            state(cx).is_some_and(|(url, alt, _)| url == invalid_url && alt == recovery_alt)
                && document_text(cx, &workspace) == original,
            "image recovery did not retain complete invalid URL and Unicode multiline alternative text: {:?}",
            state(cx)
        );
        trace.push(serde_json::json!({ "phase": "draft-recovery", "source": document_text(cx, &workspace), "inspector": state(cx) }));
        keystroke(cx, window, "escape")?;

        open(cx)?;
        rich.update(cx, |rich, cx| {
            rich.test_set_image_fields("Updated image", "replacement.png", cx)
        });
        draw(cx, window)?;
        keystroke(cx, window, "tab")?;
        keystroke(cx, window, "x")?;
        ensure!(
            state(cx)
                .is_some_and(|(url, alt, _)| url == "replacement.png" && alt == "Updated imagex"),
            "Tab and native typing did not reach the alternative-text field: {:?}",
            state(cx)
        );
        ensure!(
            document_text(cx, &workspace) == original,
            "unapplied fields changed the Markdown body"
        );
        keystroke(cx, window, "enter")?;
        let updated =
            "# Image inspector\n\n![Updated imagex](replacement.png)\n\nKeep this paragraph.\n";
        ensure!(
            state(cx).is_none(),
            "Enter did not apply and close the image inspector"
        );
        ensure!(
            document_text(cx, &workspace) == updated,
            "Apply did not atomically replace the image while preserving other text: {:?}",
            document_text(cx, &workspace)
        );
        for _ in 0..12 {
            if cx.read_entity(&rich, |rich, cx| rich.test_local_images_ready(cx)) {
                break;
            }
            draw(cx, window)?;
        }
        ensure!(
            cx.read_entity(&rich, |rich, cx| rich.test_local_images_ready(cx)),
            "relative replacement image did not reach the approved WYSIWYG body cache"
        );
        capture(
            cx,
            window,
            &workspace,
            &format!("image-editor-{}-applied-body", theme_name(theme)),
            &diagnostic_options,
            false,
        )?;
        keystroke(cx, window, "cmd-z")?;
        ensure!(
            document_text(cx, &workspace) == original,
            "Undo did not restore the entire image edit in one step"
        );
        trace.push(serde_json::json!({ "phase": "apply-and-undo", "applied": updated, "source": document_text(cx, &workspace), "inspector": state(cx) }));

        // The inspector can stay visible while its neighboring literal Source
        // pane owns input. Its visibility is not permission to steal Paste.
        keystroke(cx, window, "alt-cmd-3")?;
        open(cx)?;
        let source_geometry = cx.read_entity(&source_view, |view, _| view.painted_geometry());
        let source_point = source_geometry
            .rows
            .iter()
            .find_map(|row| {
                row.caret_stops
                    .iter()
                    .find(|(offset, _)| *offset == original.len() - 1)
                    .map(|(_, x)| point(px(*x), row.bounds.center().y))
            })
            .context("Split image fixture has no Source paragraph-end glyph stop")?;
        click_at(cx, window, source_point)?;
        let source_observation = crate::observation::capture(cx, window, &workspace)?;
        crate::observation::validate(&source_observation, original, "split")?;
        ensure!(
            source_observation.input_owner == crate::observation::InputOwner::Source,
            "visible inspector took ownership from the clicked Split Source pane"
        );
        let before_source_paste = state(cx);
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.paste(" Source paste", window, cx)
            });
        })?;
        draw(cx, window)?;
        let source_pasted = format!("{} Source paste\n", original.trim_end_matches('\n'));
        ensure!(
            document_text(cx, &workspace) == source_pasted && state(cx) == before_source_paste,
            "Split Source Paste was stolen by a visible but unfocused inspector: {:?}, {:?}",
            document_text(cx, &workspace),
            state(cx)
        );
        keystroke(cx, window, "cmd-z")?;
        ensure!(
            document_text(cx, &workspace) == original && state(cx) == before_source_paste,
            "Source Undo mutated the private image draft or failed to restore its body paste"
        );

        // Source focus is deliberately remembered here. Restoring a nested
        // field must nevertheless route app-level Undo/Redo to that field.
        cx.update_window(window.into(), |_, window, cx| {
            rich.update(cx, |rich, cx| rich.focus_current_input(window, cx))
        })?;
        draw(cx, window)?;
        keystroke(cx, window, "cmd-a")?;
        keystroke(cx, window, "x")?;
        keystroke(cx, window, "cmd-z")?;
        ensure!(
            state(cx).is_some_and(|(url, _, _)| url == "image.png")
                && document_text(cx, &workspace) == original,
            "Split URL Undo fell back to remembered Source ownership"
        );
        keystroke(cx, window, "cmd-shift-z")?;
        ensure!(
            state(cx).is_some_and(|(url, _, _)| url == "x")
                && document_text(cx, &workspace) == original,
            "Split URL Redo fell back to remembered Source ownership"
        );
        keystroke(cx, window, "tab")?;
        keystroke(cx, window, "cmd-a")?;
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.paste("Split alt", window, cx))
        })?;
        draw(cx, window)?;
        ensure!(
            state(cx).is_some_and(|(url, alt, _)| url == "x" && alt == "Split alt")
                && document_text(cx, &workspace) == original,
            "Split alternative-text Paste was misrouted to the body"
        );
        keystroke(cx, window, "cmd-z")?;
        ensure!(
            state(cx).is_some_and(|(_, alt, _)| alt == "Original image")
                && document_text(cx, &workspace) == original,
            "Split alternative-text Undo changed body history"
        );
        keystroke(cx, window, "cmd-shift-z")?;
        ensure!(
            state(cx).is_some_and(|(_, alt, _)| alt == "Split alt")
                && document_text(cx, &workspace) == original,
            "Split alternative-text Redo changed body history"
        );
        let split_observation = crate::observation::capture(cx, window, &workspace)?;
        crate::observation::validate(&split_observation, original, "split")?;
        ensure!(
            matches!(split_observation.input_owner, crate::observation::InputOwner::Widget(ref field) if field == "image-alt"),
            "Split alternative-text field did not own native input after refocus"
        );
        let (image, rich_viewport, preview_status) = cx.read_entity(&rich, |rich, cx| {
            (
                rich.test_first_image_bounds(),
                rich.painted_viewport_bounds(),
                rich.test_image_preview_status_bounds(cx),
            )
        });
        let image = image.context("Split did not observe the actual clickable image bounds")?;
        let rich_viewport = rich_viewport.context("Split lost the Rich viewport bounds")?;
        ensure!(image.left() >= rich_viewport.left() && image.right() <= rich_viewport.right()
            && image.size.width <= px(480.),
            "intrinsic-size block image overflows its narrow Rich pane: {image:?} in {rich_viewport:?}");
        let (preview, status) =
            preview_status.context("Split did not paint missing-preview status geometry")?;
        ensure!(status.left() >= preview.left() && status.right() <= preview.right()
            && status.top() >= preview.top() && status.bottom() <= preview.bottom()
            && status.size.height > px(24.),
            "missing-preview status was clipped or stayed on one overflowing line: {status:?} in {preview:?}");
        capture(
            cx,
            window,
            &workspace,
            &format!("image-editor-{}-split-owner", theme_name(theme)),
            &diagnostic_options,
            true,
        )?;
        trace.push(serde_json::json!({ "phase": "split-source-and-field-ownership", "source_paste": source_pasted, "source": document_text(cx, &workspace), "inspector": state(cx), "actual_image_bounds": format!("{image:?}"), "rich_viewport": format!("{rich_viewport:?}"), "preview_status_bounds": format!("{status:?}") }));
        keystroke(cx, window, "escape")?;
        keystroke(cx, window, "alt-cmd-1")?;

        open(cx)?;
        rich.update(cx, |rich, cx| {
            rich.test_set_image_fields("Stale draft", "replacement.png", cx)
        });
        let concurrent = format!("{original}\nConcurrent body edit.\n");
        document.update(cx, |document, cx| {
            document.replace_content(&concurrent);
            cx.notify();
        });
        draw(cx, window)?;
        keystroke(cx, window, "enter")?;
        ensure!(
            state(cx).is_some(),
            "stale image Apply closed the inspector instead of reporting the conflict"
        );
        ensure!(
            document_text(cx, &workspace) == concurrent,
            "stale image Apply overwrote a concurrent body edit"
        );
        trace.push(serde_json::json!({ "phase": "stale-apply-rejected", "source": document_text(cx, &workspace), "inspector": state(cx) }));
        keystroke(cx, window, "escape")?;
        ensure!(
            rich.update(cx, |rich, cx| rich
                .restore_recovery_widget_draft(&recovery, cx))
                == Err(markrust_editor::wysiwyg::WidgetDraftRestoreError::DocumentChanged),
            "recovery attached the image draft to a changed document"
        );
        ensure!(
            state(cx).is_none() && document_text(cx, &workspace) == concurrent,
            "rejected image recovery changed the document or opened a retargeted inspector"
        );

        cx.update_window(window.into(), |_, window, cx| {
            window.resize(size(px(680.), px(420.)));
            window.bounds_changed(cx);
        })?;
        draw(cx, window)?;
        let before_viewport = cx.read_entity(&rich, |rich, _| rich.test_viewport_state());
        open(cx)?;
        let (panel, viewport) = cx.read_entity(&rich, |rich, _| {
            (
                rich.painted_image_editor_bounds(),
                rich.painted_viewport_bounds(),
            )
        });
        let panel = panel.context("compact image inspector did not paint its bounds")?;
        let viewport = viewport.context("compact image inspector lost its document viewport")?;
        ensure!(
            panel.left() >= viewport.left()
                && panel.right() <= viewport.right()
                && panel.top() >= viewport.top()
                && panel.bottom() <= viewport.bottom(),
            "image inspector escapes the short document viewport: {panel:?} inside {viewport:?}"
        );
        let (apply_button, cancel_button) = cx
            .read_entity(&rich, |rich, cx| rich.test_image_action_bounds(cx))
            .context("compact image inspector did not paint both action hit targets")?;
        for (name, button) in [("Apply", apply_button), ("Cancel", cancel_button)] {
            ensure!(
                button.size.width > px(0.)
                    && button.size.height > px(0.)
                    && button.left() >= panel.left()
                    && button.right() <= panel.right()
                    && button.top() >= panel.top()
                    && button.bottom() <= panel.bottom(),
                "compact {name} button clipped outside the inspector: {button:?} in {panel:?}"
            );
        }
        let after_viewport = cx.read_entity(&rich, |rich, _| rich.test_viewport_state());
        ensure!(
            before_viewport.0.item_ix == after_viewport.0.item_ix
                && before_viewport.0.offset_in_item == after_viewport.0.offset_in_item
                && before_viewport.1 == after_viewport.1,
            "opening the image inspector changed the document scroll anchor or viewport"
        );
        capture(
            cx,
            window,
            &workspace,
            &format!("image-editor-{}-compact", theme_name(theme)),
            &diagnostic_options,
            false,
        )?;
        let (scroll_bounds, scroll_before, preview_before) = cx
            .read_entity(&rich, |rich, cx| rich.test_image_scroll_state(cx))
            .context("compact inspector did not expose its actual middle scroll handle")?;
        let wheel_position = point(scroll_bounds.left() + px(8.), scroll_bounds.top() + px(4.));
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_mouse_move(wheel_position, cx);
            window.dispatch_event(
                PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                    position: wheel_position,
                    delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-96.))),
                    modifiers: Modifiers::default(),
                    touch_phase: gpui::TouchPhase::Moved,
                }),
                cx,
            );
        })?;
        draw(cx, window)?;
        let (scroll_bounds_after, scroll_after, preview_after) = cx
            .read_entity(&rich, |rich, cx| rich.test_image_scroll_state(cx))
            .context("compact middle scroll state disappeared after the native wheel")?;
        let footer_after = cx.read_entity(&rich, |rich, cx| rich.test_image_action_bounds(cx));
        let rich_after_wheel = cx.read_entity(&rich, |rich, _| rich.test_viewport_state());
        trace.push(serde_json::json!({"phase":"native-compact-middle-scroll-observed", "pointer":format!("{wheel_position:?}"), "bounds_before":format!("{scroll_bounds:?}"), "bounds_after":format!("{scroll_bounds_after:?}"), "offset_before":format!("{scroll_before:?}"), "offset_after":format!("{scroll_after:?}"), "footer_before":format!("{:?}", (apply_button, cancel_button)), "footer_after":format!("{footer_after:?}"), "rich_anchor_before":format!("{:?}", after_viewport.0), "rich_anchor_after":format!("{:?}", rich_after_wheel.0), "rich_bounds_before":format!("{:?}", after_viewport.1), "rich_bounds_after":format!("{:?}", rich_after_wheel.1)}));
        ensure!(scroll_after.y < scroll_before.y && scroll_bounds == scroll_bounds_after
            && footer_after == Some((apply_button, cancel_button))
            && rich_after_wheel.0.item_ix == after_viewport.0.item_ix
            && rich_after_wheel.0.offset_in_item == after_viewport.0.offset_in_item
            && rich_after_wheel.1 == after_viewport.1,
            "native inspector wheel failed to scroll only the middle, or moved the footer/document viewport: pointer={wheel_position:?}, scroll={scroll_before:?}->{scroll_after:?}, middle={scroll_bounds:?}->{scroll_bounds_after:?}, footer={:?}->{footer_after:?}, Rich anchor={:?}->{:?}, Rich bounds={:?}->{:?}",
            (apply_button, cancel_button), after_viewport.0, rich_after_wheel.0, after_viewport.1, rich_after_wheel.1);
        if let (Some(before), Some(after)) = (preview_before, preview_after) {
            ensure!(
                after.top() < before.top(),
                "middle scrolled but the preview did not move"
            );
        }
        capture(
            cx,
            window,
            &workspace,
            &format!("image-editor-{}-compact-scrolled", theme_name(theme)),
            &diagnostic_options,
            false,
        )?;
        trace.push(serde_json::json!({"phase":"native-compact-middle-scroll", "before":format!("{scroll_before:?}"), "after":format!("{scroll_after:?}"), "fixed_apply":format!("{apply_button:?}"), "fixed_cancel":format!("{cancel_button:?}")}));
        click_at(cx, window, cancel_button.center())?;
        ensure!(
            state(cx).is_none() && document_text(cx, &workspace) == concurrent,
            "compact inspector Cancel lost the concurrent document text"
        );
        trace.push(serde_json::json!({ "phase": "compact-cancel", "source": document_text(cx, &workspace), "panel": format!("{panel:?}"), "viewport": format!("{viewport:?}") }));

        open(cx)?;
        rich.update(cx, |rich, cx| {
            rich.test_set_image_fields("Compact image", "replacement.png", cx)
        });
        draw(cx, window)?;
        let (apply_button, _) = cx
            .read_entity(&rich, |rich, cx| rich.test_image_action_bounds(cx))
            .context("compact Apply did not retain a painted hit target")?;
        click_at(cx, window, apply_button.center())?;
        let compact_applied = concurrent.replace(
            "![Original image](image.png)",
            "![Compact image](replacement.png)",
        );
        ensure!(
            state(cx).is_none() && document_text(cx, &workspace) == compact_applied,
            "native compact Apply did not commit the intended image atomically"
        );
        keystroke(cx, window, "cmd-z")?;
        ensure!(
            document_text(cx, &workspace) == concurrent,
            "compact image Apply did not undo in one step"
        );

        // The fallback must expose the same real click path as decoded pixels.
        let missing = "![Missing image](missing.png)\n";
        document.update(cx, |document, cx| {
            document.replace_content(missing);
            cx.notify();
        });
        draw(cx, window)?;
        open(cx)?;
        ensure!(
            state(cx).is_some_and(|(url, alt, approved)| url == "missing.png"
                && alt == "Missing image"
                && !approved),
            "native missing-image click did not open the intended properties"
        );
        keystroke(cx, window, "escape")?;
        ensure!(
            document_text(cx, &workspace) == missing && state(cx).is_none(),
            "missing-image inspector Cancel mutated its source"
        );
        trace.push(serde_json::json!({ "phase": "native-compact-apply-and-missing-image-click", "applied": compact_applied, "source": document_text(cx, &workspace) }));
        Ok(())
    })();
    // Always release synthetic windows, including failed fixtures, before
    // returning an assertion; otherwise a leaked entity hides the real fault.
    let trace_result: Result<()> = (|| {
        std::fs::write(
            directory.join("states.json"),
            serde_json::to_vec_pretty(&trace)?,
        )?;
        Ok(())
    })();
    let close_result = cx.update_window(window.into(), |_, window, _| window.remove_window());
    drop(document);
    drop(rich);
    drop(source_view);
    drop(workspace);
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    result?;
    trace_result?;
    close_result?;
    println!("PASS image-editor-{} (native raster/placeholder clicks, relative preview, native fields and field Undo/Redo, Split Source/inspector ownership, tab return, draft recovery, Escape Cancel, Enter Apply, atomic document Undo, stale guards, compact action hit targets)", theme_name(theme));
    Ok(8)
}

/// Cache approval is not proof that the GPU has painted the image. The fixture
/// has an orange-red hash on a neutral background; inspect only the preview box
/// so the body image and orange toolbar/inspector controls cannot satisfy this.
fn wait_for_image_preview_raster(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    rich: &Entity<RichEditorView>,
    options: &Options,
    label: &str,
) -> Result<usize> {
    if options.geometry_only {
        return Ok(0);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut latest = None;
    let mut latest_count = 0;
    let mut latest_bounds = None;
    for _ in 0..24 {
        draw(cx, window)?;
        let (bounds, scale) = cx.update_window(window.into(), |_, window, cx| {
            (
                rich.read(cx)
                    .test_image_scroll_state(cx)
                    .and_then(|(_, _, preview)| preview),
                window.scale_factor(),
            )
        })?;
        let screenshot = cx
            .capture_screenshot(window.into())
            .context("Metal image-preview readiness capture failed")?;
        latest_count = bounds.map_or(0, |bounds| orange_raster_pixels(&screenshot, bounds, scale));
        if latest_count >= 32 {
            return Ok(latest_count);
        }
        latest = Some(screenshot);
        latest_bounds = bounds;
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
    if let Some(screenshot) = latest {
        save_screenshot(
            &screenshot,
            &options.output.join(format!("{label}-raster-timeout.png")),
        )?;
    }
    bail!("{label}: approved image did not visibly paint in the preview within the bounded frame wait: orange pixels={latest_count}, preview={latest_bounds:?}");
}

fn orange_raster_pixels(
    screenshot: &RgbaImage,
    bounds: gpui::Bounds<gpui::Pixels>,
    scale: f32,
) -> usize {
    let left = (f32::from(bounds.left()) * scale).max(0.).floor() as u32;
    let top = (f32::from(bounds.top()) * scale).max(0.).floor() as u32;
    let right = ((f32::from(bounds.right()) * scale).max(0.).ceil() as u32).min(screenshot.width());
    let bottom =
        ((f32::from(bounds.bottom()) * scale).max(0.).ceil() as u32).min(screenshot.height());
    (top..bottom)
        .flat_map(|y| (left..right).map(move |x| screenshot.get_pixel(x, y)))
        .filter(|pixel| {
            pixel[3] > 220
                && pixel[0] > 150
                && pixel[1] < 130
                && pixel[2] < 100
                && i16::from(pixel[0]) - i16::from(pixel[1]) > 70
        })
        .count()
}

fn click_at(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    position: gpui::Point<gpui::Pixels>,
) -> Result<()> {
    cx.update_window(window.into(), |_, window, cx| {
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
    keystroke(cx, window, "ctrl-cmd-s")?;
    let before_editing = crate::observation::capture(cx, window, workspace)?;
    keystroke(cx, window, "right")?;
    let after_editing = crate::observation::capture(cx, window, workspace)?;
    ensure!(
        after_editing.overlay.is_none()
            && before_editing.source_pane.viewport == after_editing.source_pane.viewport
            && before_editing.rich_pane.viewport == after_editing.rich_pane.viewport,
        "keyboard editing did not dismiss floating Sidebar without resizing either pane"
    );
    // Continue the outside click into the underlying editor after dismissal.
    keystroke(cx, window, "ctrl-cmd-o")?;
    let before_click = crate::observation::capture(cx, window, workspace)?;
    let viewport = before_click
        .rich_pane
        .viewport
        .as_ref()
        .context("Split rich viewport")?;
    let position = point(px(viewport.x + 12.), px(viewport.y + 18.));
    cx.update_window(window.into(), |_, window, cx| {
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
    let after_click = crate::observation::capture(cx, window, workspace)?;
    ensure!(
        after_click.overlay.is_none()
            && before_click.source_pane.viewport == after_click.source_pane.viewport
            && before_click.rich_pane.viewport == after_click.rich_pane.viewport,
        "editor click did not dismiss floating Outline without resizing either pane"
    );
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
    // Tabs now belong to the editor column; the Outline begins directly below
    // the fixed 46/36px toolbar rows, not below a global document-tab strip.
    let position = point(px(width - OUTLINE_WIDTH + 48.), px(160.));
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
                && tab.editor.read(cx).raw_source()
        })?,
        "new tab did not enter literal Source mode with editor focus"
    );

    // Hit the real first tab bounds, independent of sidebar/chrome placement.
    let root = window.root(cx)?;
    let first_bounds = cx
        .read_entity(&root, |root, _| root.test_tab_strip_state(0).1)
        .context("first document tab has no painted bounds")?;
    let position = point(
        first_bounds.left() + px(40.),
        first_bounds.top() + first_bounds.size.height / 2.,
    );
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

fn type_fixture_text(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    text: &str,
) -> Result<()> {
    for character in text.chars() {
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
    }
    draw(cx, window)
}

fn scroll_fixture_pane(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<MarkRustWindow>,
    viewport: gpui::Bounds<gpui::Pixels>,
) -> Result<()> {
    cx.update_window(window.into(), |_, window, cx| {
        window.simulate_mouse_move(viewport.center(), cx);
        window.dispatch_event(
            PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                position: viewport.center(),
                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-80.))),
                modifiers: Modifiers::default(),
                touch_phase: gpui::TouchPhase::Moved,
            }),
            cx,
        );
    })?;
    draw(cx, window)
}

/// Native input and real filesystem validation, using synthetic evidence files
/// only. The original unsaved tab must survive every error and successful open.
fn check_open_path(
    cx: &mut HeadlessAppContext,
    theme: ThemeChoice,
    options: &Options,
) -> Result<usize> {
    let options = Options {
        baseline: None,
        update_baselines: false,
        ..options.clone()
    };
    let directory = options.output.join(format!(
        "open-path-{}-{}",
        theme_name(theme),
        std::process::id()
    ));
    std::fs::create_dir_all(&directory)?;
    let directory = std::fs::canonicalize(directory)?;
    let named = directory.join("пример document.md");
    let named_text = "# Open Location\n\nA synthetic file, not a user document.\n";
    std::fs::write(&named, named_text)?;
    let draft = "# Private draft\n\nDo not replace this unsaved buffer.\n";
    let (window, workspace) = open_fixture(cx, draft, theme)?;
    let result: Result<usize> = (|| {
        keystroke(cx, window, "alt-cmd-3")?;
        cx.update_window(window.into(), |_, window, cx| {
            let editor = workspace.read(cx).active_tab().unwrap().editor.clone();
            editor.update(cx, |editor, cx| editor.jump_to(4, cx));
            let focus = editor.read(cx).focus_handle.clone();
            focus.focus(window, cx);
        })?;
        draw(cx, window)?;
        let original_id = cx.read_entity(&workspace, |workspace, _| {
            workspace.active_tab().unwrap().id
        });
        let before = crate::observation::capture(cx, window, &workspace)?;

        keystroke(cx, window, "cmd-shift-l")?;
        let opened = crate::observation::capture(cx, window, &workspace)?;
        crate::observation::validate(&opened, draft, "split")?;
        let input = opened
            .open_path
            .as_ref()
            .context("Open Location shortcut did not open its field")?
            .input_bounds
            .as_ref()
            .context("Open Location field was not painted")?;
        let position = point(
            px(input.x + input.width / 2.),
            px(input.y + input.height / 2.),
        );
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }),
                cx,
            );
        })?;
        draw(cx, window)?;
        keystroke(cx, window, "cmd-a")?;
        let missing = directory.join("does-not-exist.md");
        type_fixture_text(cx, window, &missing.to_string_lossy())?;
        keystroke(cx, window, "enter")?;
        let error = crate::observation::capture(cx, window, &workspace)?;
        crate::observation::validate(&error, draft, "split")?;
        ensure!(error.open_path.as_ref().is_some_and(|path| path.error.is_some()
            && path.query == missing.to_string_lossy()), "invalid path lost its inline error or field draft");
        ensure!(
            document_text(cx, &workspace) == draft && error.tab_count == 1,
            "invalid Open Location path replaced the unsaved document"
        );
        capture(
            cx,
            window,
            &workspace,
            &format!("open-path-{}-inline-error", theme_name(theme)),
            &options,
            true,
        )?;
        keystroke(cx, window, "escape")?;
        let cancelled = crate::observation::capture(cx, window, &workspace)?;
        ensure!(
            cancelled.open_path.is_none()
                && cancelled.input_owner == crate::observation::InputOwner::Source
                && cancelled.source_pane.selection == before.source_pane.selection,
            "Escape from Open Location lost the originating Source caret"
        );

        keystroke(cx, window, "cmd-shift-l")?;
        keystroke(cx, window, "cmd-a")?;
        type_fixture_text(cx, window, &named.to_string_lossy())?;
        let ready = crate::observation::capture(cx, window, &workspace)?;
        crate::observation::validate(&ready, draft, "split")?;
        capture(
            cx,
            window,
            &workspace,
            &format!("open-path-{}-ready", theme_name(theme)),
            &options,
            true,
        )?;
        keystroke(cx, window, "enter")?;
        ensure!(
            cx.read_entity(&workspace, |workspace, cx| {
                workspace.tabs.len() == 2
                    && workspace
                        .active_tab()
                        .unwrap()
                        .document
                        .read(cx)
                        .path
                        .as_ref()
                        == Some(&named)
                    && workspace
                        .tabs
                        .iter()
                        .find(|tab| tab.id == original_id)
                        .is_some_and(|tab| tab.document.read(cx).buffer.content() == draft)
            }),
            "Open Location did not open the synthetic file in a new tab while preserving the draft"
        );
        ensure!(
            document_text(cx, &workspace) == named_text,
            "Open Location loaded a different file's contents"
        );
        let file = crate::observation::capture(cx, window, &workspace)?;
        ensure!(
            file.open_path.is_none(),
            "successful file open left the modal active"
        );
        capture(
            cx,
            window,
            &workspace,
            &format!("open-path-{}-file-open", theme_name(theme)),
            &options,
            true,
        )?;

        keystroke(cx, window, "cmd-shift-l")?;
        keystroke(cx, window, "cmd-a")?;
        type_fixture_text(cx, window, &directory.to_string_lossy())?;
        keystroke(cx, window, "enter")?;
        ensure!(
            cx.read_entity(&workspace, |workspace, cx| {
                workspace.root.as_ref() == Some(&directory)
                    && workspace.list_files().contains(&named)
                    && workspace
                        .tabs
                        .iter()
                        .find(|tab| tab.id == original_id)
                        .is_some_and(|tab| tab.document.read(cx).buffer.content() == draft)
            }),
            "Open Location folder did not expose its file list or preserved unsaved draft"
        );
        keystroke(cx, window, "ctrl-cmd-s")?;
        let folder = crate::observation::capture(cx, window, &workspace)?;
        ensure!(
            folder.sidebar
                && cx.read_entity(&workspace, |workspace, _| {
                    workspace.sidebar_open && workspace.root.as_ref() == Some(&directory)
                }),
            "Open Location folder evidence did not show the active folder sidebar"
        );
        capture(
            cx,
            window,
            &workspace,
            &format!("open-path-{}-folder-open", theme_name(theme)),
            &options,
            true,
        )?;
        println!(
            "PASS open-path-{} (native field, pointer focus, error, Escape, file and folder)",
            theme_name(theme)
        );
        Ok(4)
    })();
    cx.update_window(window.into(), |_, window, _| window.remove_window())?;
    drop(workspace);
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    result
}

/// Live locale/theme render with a production menu-model contract. GPUI's
/// headless platform does not install OS menus; this is not OS-menu/RTL or IME
/// certification. Screenshots retain native shaping evidence for manual review.
fn check_ui_language(
    cx: &mut HeadlessAppContext,
    theme: ThemeChoice,
    options: &Options,
) -> Result<usize> {
    use crate::i18n::Language;
    let options = Options {
        baseline: None,
        update_baselines: false,
        ..options.clone()
    };
    let content = format!(
        "# Locale fixture\n\nЭто café 👩🏽‍💻 remains unchanged.\n\n{}",
        crate::usecases::DocumentTemplate::DeepPointer.render()
    );
    let deep_caret = content.find("Section 060").unwrap() + 8;
    let (window, workspace) = open_fixture(cx, &content, theme)?;
    let result: Result<usize> = (|| {
        keystroke(cx, window, "alt-cmd-3")?;
        cx.update_window(window.into(), |_, window, cx| {
            let rich = workspace.read(cx).active_tab().unwrap().rich_view.clone();
            rich.update(cx, |rich, cx| rich.jump_to(deep_caret, cx));
            rich.read(cx).focus_handle(cx).focus(window, cx);
        })?;
        draw(cx, window)?;
        let rich_viewport = cx.read_entity(&workspace, |workspace, cx| {
            workspace
                .active_tab()
                .unwrap()
                .rich_view
                .read(cx)
                .test_viewport_state()
                .1
        });
        scroll_fixture_pane(cx, window, rich_viewport)?;
        cx.update_window(window.into(), |_, window, cx| {
            let editor = workspace.read(cx).active_tab().unwrap().editor.clone();
            editor.update(cx, |editor, cx| {
                editor.apply_command(
                    markrust_editor::EditorCommand::SetSelection {
                        start: deep_caret,
                        end: deep_caret + 3,
                    },
                    cx,
                )
            });
            let focus = editor.read(cx).focus_handle.clone();
            focus.focus(window, cx);
        })?;
        draw(cx, window)?;
        let source_viewport = source_scroll_state(cx, &workspace).0;
        scroll_fixture_pane(cx, window, source_viewport)?;
        let original = crate::observation::capture(cx, window, &workspace)?;
        let scroll_state = |cx: &HeadlessAppContext| {
            cx.read_entity(&workspace, |workspace, cx| {
                let tab = workspace.active_tab().unwrap();
                let rich = tab.rich_view.read(cx).test_viewport_state().0;
                (
                    rich.item_ix,
                    rich.offset_in_item,
                    tab.editor_view.read(cx).horizontal_scroll_state().2,
                )
            })
        };
        let deep_scroll = scroll_state(cx);
        ensure!(
            deep_scroll.0 > 0 && f32::from(deep_scroll.2.y) < -100.,
            "locale fixture failed to manually scroll both deep document panes"
        );
        let languages = [
            (Language::Russian, "ru", "Файл"),
            (Language::Japanese, "ja", "ファイル"),
            (Language::Arabic, "ar", "ملف"),
            (Language::English, "en", "File"),
        ];
        let mut count = 0;
        for with_find in [false, true] {
            if with_find {
                keystroke(cx, window, "cmd-f")?;
                keystroke(cx, window, "cmd-a")?;
                type_fixture_text(cx, window, "café")?;
            }
            let before = crate::observation::capture(cx, window, &workspace)?;
            let before_scroll = scroll_state(cx);
            for (language, code, expected_file_menu) in languages {
                workspace.update(cx, |workspace, cx| workspace.set_ui_language(language, cx));
                draw(cx, window)?;
                let observed = crate::observation::capture(cx, window, &workspace)?;
                crate::observation::validate(&observed, &content, "split")?;
                ensure!(
                    document_text(cx, &workspace) == content
                        && observed.document_revision == original.document_revision
                        && observed.source_pane.selection == original.source_pane.selection
                        && observed.rich_pane.selection == original.rich_pane.selection,
                    "runtime language update rewrote document bytes, revision or real selection"
                );
                ensure!(scroll_state(cx) == before_scroll,
                    "runtime language update moved a manually scrolled pane or revealed an old caret");
                ensure!(
                    observed.input_owner == before.input_owner,
                    "runtime language update stole native input ownership"
                );
                if with_find {
                    ensure!(
                        observed.find.as_ref().is_some_and(|find| find.focused
                            && find.query == "café"
                            && find.matches.len() == 1),
                        "runtime language update lost the live Find query"
                    );
                }
                // TestWindow.is_active() is deliberately false and its menu
                // setter is a no-op. Generate the production menu model from
                // this live workspace instead of claiming OS-menu activation.
                let menu_state = cx.read_entity(&workspace, |workspace, cx| {
                    let tab = workspace.active_tab().unwrap();
                    ensure!(
                        tab.editor.read(cx).theme.ui_text("File").as_str() == expected_file_menu
                            && tab.rich_view.read(cx).theme.ui_text("File").as_str()
                                == expected_file_menu,
                        "locale did not propagate its catalog to both real editor themes"
                    );
                    Ok::<_, anyhow::Error>(crate::menus::MenuState {
                        language: workspace.config.language,
                        automatic_updates: workspace.config.automatic_updates,
                        mode: tab.mode,
                        sidebar_open: workspace.sidebar_open,
                        outline_open: workspace.outline_open,
                        markup_hints_enabled: workspace.config.markup_hints_enabled,
                        highlight_style: workspace.config.highlight_style,
                    })
                })?;
                ensure!(
                    menu_state.language == language,
                    "live workspace language did not reach the production menu model"
                );
                let menu_names: Vec<_> = crate::menus::application_menus(menu_state)
                    .into_iter()
                    .map(|menu| menu.name.to_string())
                    .collect();
                ensure!(menu_names.iter().any(|name| name == expected_file_menu),
                    "native render's menu model has no independently expected localized File label: {menu_names:?}");
                let label = format!(
                    "locale-{}-{code}-{}",
                    theme_name(theme),
                    if with_find { "find" } else { "body" }
                );
                capture(cx, window, &workspace, &label, &options, true)?;
                std::fs::write(
                    options.output.join(format!("{label}.menus.json")),
                    serde_json::to_vec_pretty(&menu_names)?,
                )?;
                count += 1;
            }
        }
        keystroke(cx, window, "escape")?;
        ensure!(
            crate::observation::capture(cx, window, &workspace)?.input_owner
                == crate::observation::InputOwner::Source,
            "Find Escape after locale roundtrip did not restore Source input"
        );
        println!("PASS locale-{} (RU/JA/AR/EN live render and menu model; not OS-menu/RTL/IME certification)", theme_name(theme));
        Ok(count)
    })();
    cx.update_window(window.into(), |_, window, _| window.remove_window())?;
    drop(workspace);
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    result
}

/// Exercise the real Workspace restore/observer path with synthetic files only.
fn check_session_recovery(
    cx: &mut HeadlessAppContext,
    theme: ThemeChoice,
    options: &Options,
) -> Result<usize> {
    // Recovery warnings include per-run temporary paths. Keep their images
    // as diagnostic evidence, not environment-dependent golden references.
    let recovery_options = Options {
        baseline: None,
        update_baselines: false,
        ..options.clone()
    };
    use crate::recovery::{
        RecoveryEditingPane, RecoveryEditorMode, RecoverySelection, RecoverySnapshot,
        RecoveryStore, RecoveryTab, RECOVERY_VERSION,
    };
    let fixture_dir = options.output.join(format!(
        "recovery-{}-{}",
        theme_name(theme),
        std::process::id()
    ));
    std::fs::create_dir(&fixture_dir)?;
    let named_path = fixture_dir.join("changed-on-disk.md");
    let external = "# External version\n";
    let recovered = "# Local edit\n\nNever lose this.\n";
    std::fs::write(&named_path, external)?;
    let store = RecoveryStore::new(fixture_dir.join("private-session"));
    let draft = "Draft 👋";
    let make_tab = |path, title: &str, content: &str, base: &str, mode, pane| RecoveryTab {
        widget_draft: None,
        path,
        title: title.into(),
        content: content.into(),
        saved_content: base.into(),
        dirty: true,
        autosave_blocked: false,
        mode,
        editing_pane: pane,
        source_selection: RecoverySelection {
            start: content.len(),
            end: content.len(),
            reversed: false,
        },
        rich_selection: RecoverySelection {
            start: content.len(),
            end: content.len(),
            reversed: false,
        },
    };
    store.write(&RecoverySnapshot {
        version: RECOVERY_VERSION,
        root: None,
        active_tab: 1,
        archived_tabs: vec![],
        tabs: vec![
            make_tab(
                None,
                "Untitled",
                draft,
                "",
                RecoveryEditorMode::Wysiwyg,
                RecoveryEditingPane::Wysiwyg,
            ),
            make_tab(
                Some(named_path.clone()),
                "changed-on-disk.md",
                recovered,
                "# Original version\n",
                RecoveryEditorMode::Split,
                RecoveryEditingPane::Source,
            ),
        ],
    })?;
    let (window, workspace) = open_recovery_fixture(cx, theme, store.clone())?;
    draw(cx, window)?;
    let observation = crate::observation::capture(cx, window, &workspace)?;
    ensure!(
        observation.tab_count == 2
            && observation.input_owner == crate::observation::InputOwner::Source,
        "session restore lost tab order or Split input owner"
    );
    ensure!(
        document_text(cx, &workspace) == recovered
            && observation.source_pane.caret == recovered.len(),
        "session restore lost dirty buffer or caret"
    );
    ensure!(
        cx.read_entity(&workspace, |workspace, _| matches!(
            workspace.recovery_warning(),
            Some(crate::recovery::RecoveryWarning::DiskChanged(_))
        )),
        "restored disk conflict did not expose a visible recovery warning"
    );
    keystroke(cx, window, "x")?;
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    draw(cx, window)?;
    ensure!(
        std::fs::read_to_string(&named_path)? == external,
        "recovered buffer autosaved over a newer disk version"
    );
    capture(
        cx,
        window,
        &workspace,
        &format!("recovery-{}-split", theme_name(theme)),
        &recovery_options,
        true,
    )?;
    keystroke(cx, window, "ctrl-shift-tab")?;
    ensure!(
        document_text(cx, &workspace) == draft,
        "restored untitled draft was lost"
    );
    keystroke(cx, window, "y")?;
    workspace.update(cx, |workspace, cx| workspace.flush_recovery(cx));
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    draw(cx, window)?;
    let checkpoint = store
        .load()
        .snapshot
        .context("native typing produced no recovery checkpoint")?;
    ensure!(
        checkpoint.tabs[0].content == format!("{draft}y")
            && checkpoint.tabs[1].content == format!("{recovered}x")
            && checkpoint.active_tab == 0,
        "native document observers did not persist both edited buffers"
    );
    // A dirty named draft may be closed and its original reopened. Recovery
    // must not later produce two writable owners of that same path.
    cx.update_window(window.into(), |_, window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.close_tab(1, window, cx);
            workspace.open_document(named_path.clone(), window, cx)?;
            workspace.flush_recovery(cx);
            Ok::<(), anyhow::Error>(())
        })
    })??;
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    draw(cx, window)?;
    cx.update_window(window.into(), |_, window, _| window.remove_window())?;
    drop(workspace);
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    // A restart creates a fresh store/epoch; only the files survive.
    let (window, workspace) = open_recovery_fixture(
        cx,
        theme,
        RecoveryStore::new(fixture_dir.join("private-session")),
    )?;
    draw(cx, window)?;
    ensure!(
        cx.read_entity(&workspace, |workspace, cx| {
            workspace.tabs.len() == 3
                && workspace.tabs[2].document.read(cx).path.is_none()
                && workspace.tabs[2].document.read(cx).dirty
        }),
        "closed named draft did not recover as a separate Save As buffer"
    );
    keystroke(cx, window, "ctrl-tab")?;
    ensure!(
        document_text(cx, &workspace) == format!("{recovered}x"),
        "closed dirty named draft was lost on restart"
    );
    keystroke(cx, window, "z")?;
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    draw(cx, window)?;
    ensure!(
        std::fs::read_to_string(&named_path)? == external,
        "archived named draft overwrote the live file"
    );
    keystroke(cx, window, "ctrl-tab")?;
    ensure!(
        document_text(cx, &workspace) == format!("{draft}y"),
        "second restart lost untitled typing"
    );
    let observation = crate::observation::capture(cx, window, &workspace)?;
    crate::observation::validate(&observation, &document_text(cx, &workspace), "wysiwyg")?;
    capture(
        cx,
        window,
        &workspace,
        &format!("recovery-{}-untitled", theme_name(theme)),
        &recovery_options,
        true,
    )?;
    cx.update_window(window.into(), |_, window, _| window.remove_window())?;
    drop(workspace);
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    println!(
        "PASS session-recovery-{} (native typing, restart, dirty buffers, Split focus, newer-disk guard)",
        theme_name(theme)
    );
    Ok(2)
}

fn open_recovery_fixture(
    cx: &mut HeadlessAppContext,
    theme: ThemeChoice,
    store: crate::recovery::RecoveryStore,
) -> Result<(WindowHandle<MarkRustWindow>, Entity<Workspace>)> {
    let mut workspace_handle = None;
    let window = cx.open_window(size(px(WIDTHS[0]), px(HEIGHT)), |window, cx| {
        let workspace = cx.new(|cx| {
            let mut workspace = Workspace::new_for_recovery_tests(
                AppConfig {
                    theme,
                    ..AppConfig::default()
                },
                store,
                window,
                cx,
            );
            workspace.sidebar_open = false;
            workspace.outline_open = false;
            for tab in &workspace.tabs {
                tab.document.update(cx, |document, _| {
                    assert!(
                        document.wait_for_parse(Duration::from_secs(5)),
                        "recovery parse timed out"
                    );
                });
            }
            workspace
        });
        workspace_handle = Some(workspace.clone());
        cx.new(|cx| MarkRustWindow::new(workspace, cx))
    })?;
    Ok((
        window,
        workspace_handle.context("recovery fixture was not created")?,
    ))
}

/// Saving/closing must capture the visible widget draft, not the prior URL.
fn check_pending_widget_persistence(
    cx: &mut HeadlessAppContext,
    theme: ThemeChoice,
    options: &Options,
) -> Result<()> {
    use crate::recovery::{
        RecoveryEditingPane, RecoveryEditorMode, RecoverySelection, RecoverySnapshot,
        RecoveryStore, RecoveryTab, RECOVERY_VERSION,
    };
    let directory = options.output.join(format!(
        "widget-persistence-{}-{}",
        theme_name(theme),
        std::process::id()
    ));
    std::fs::create_dir(&directory)?;
    let path = directory.join("links.md");
    // Canonical final newline avoids the separate AppKit Normalize dialog.
    // That operating-system interaction remains a native-window acceptance gate.
    let initial = "A [link](https://old.example) stays.\n";
    std::fs::write(&path, initial)?;
    let store = RecoveryStore::new(directory.join("private-session"));
    let make_tab = |path, title: &str, content: &str, caret| RecoveryTab {
        widget_draft: None,
        path,
        title: title.into(),
        content: content.into(),
        saved_content: content.into(),
        dirty: false,
        autosave_blocked: false,
        mode: RecoveryEditorMode::Wysiwyg,
        editing_pane: RecoveryEditingPane::Wysiwyg,
        source_selection: RecoverySelection {
            start: caret,
            end: caret,
            reversed: false,
        },
        rich_selection: RecoverySelection {
            start: caret,
            end: caret,
            reversed: false,
        },
    };
    store.write(&RecoverySnapshot {
        version: RECOVERY_VERSION,
        root: None,
        active_tab: 0,
        tabs: vec![
            make_tab(
                Some(path.clone()),
                "links.md",
                initial,
                initial.find("link]").unwrap() + 2,
            ),
            make_tab(None, "Untitled", "", 0),
        ],
        archived_tabs: vec![],
    })?;
    let (window, workspace) = open_recovery_fixture(cx, theme, store.clone())?;
    draw(cx, window)?;
    keystroke(cx, window, "cmd-k")?;
    // The existing URL is selected when the editor opens.
    keystroke(cx, window, "n")?;
    keystroke(cx, window, "cmd-s")?;
    draw(cx, window)?;
    let saved = initial.replace("https://old.example", "n");
    ensure!(
        std::fs::read_to_string(&path)? == saved,
        "Cmd-S did not commit the visible URL draft before saving"
    );
    ensure!(
        cx.read_entity(&workspace, |workspace, cx| workspace
            .active_tab()
            .unwrap()
            .rich_view
            .read(cx)
            .test_widget_kind()
            .is_none()),
        "Cmd-S left a successfully saved URL draft open"
    );
    keystroke(cx, window, "cmd-k")?;
    keystroke(cx, window, "m")?;
    // A true tab-close key exercises archive ordering before focus changes.
    keystroke(cx, window, "cmd-w")?;
    workspace.update(cx, |workspace, cx| workspace.flush_recovery(cx));
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let checkpoint = store
        .load()
        .snapshot
        .context("widget close produced no checkpoint")?;
    ensure!(
        checkpoint.tabs.len() == 1
            && checkpoint.archived_tabs.len() == 1
            && checkpoint.archived_tabs[0].content == initial.replace("https://old.example", "m"),
        "tab close archived the prior URL instead of the visible draft"
    );
    ensure!(
        std::fs::read_to_string(&path)? == saved,
        "closing the URL draft unexpectedly autosaved over its last explicit save"
    );
    cx.update_window(window.into(), |_, window, _| window.remove_window())?;
    drop(workspace);
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let (window, workspace) = open_recovery_fixture(
        cx,
        theme,
        RecoveryStore::new(directory.join("private-session")),
    )?;
    draw(cx, window)?;
    keystroke(cx, window, "ctrl-tab")?;
    ensure!(
        document_text(cx, &workspace) == initial.replace("https://old.example", "m"),
        "closed URL draft was lost after restarting the session"
    );
    ensure!(
        cx.read_entity(&workspace, |workspace, cx| workspace
            .active_tab()
            .unwrap()
            .document
            .read(cx)
            .path
            .is_none()),
        "closed URL draft recovered as a duplicate writable file"
    );
    cx.update_window(window.into(), |_, window, _| window.remove_window())?;
    drop(workspace);
    cx.advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    println!(
        "PASS pending-widget-persistence-{} (Cmd-S, close archive, restart)",
        theme_name(theme)
    );
    Ok(())
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

/// Lossless diagnostic frames favor capture latency over smallest file size.
/// Adaptive PNG filtering is needlessly expensive in an unoptimized test build.
pub(crate) fn save_screenshot(screenshot: &RgbaImage, path: &Path) -> Result<()> {
    use std::io::Write;
    let mut output = std::io::BufWriter::new(std::fs::File::create(path)?);
    encode_screenshot(screenshot, &mut output)?;
    output.flush()?;
    Ok(())
}

fn encode_screenshot(screenshot: &RgbaImage, output: impl std::io::Write) -> Result<()> {
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        output,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Sub,
    );
    encoder.write_image(
        screenshot.as_raw(),
        screenshot.width(),
        screenshot.height(),
        image::ExtendedColorType::Rgba8,
    )?;
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
        save_screenshot(&screenshot, &options.output.join(format!("{label}.png")))?;
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
        ensure!(
            f32::from(pane.size.width) >= minimum_document_width,
            "{label}: document pane is only {:.1}px wide; {:?} needs at least {minimum_document_width}px at supported window sizes",
            f32::from(pane.size.width),
            mode
        );
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
                save_screenshot(&screenshot, &path)?;
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
        std::fs::write(
            options.output.join(format!("{label}.selection.txt")),
            format!(
                "source selection: {selection:?}\nviewport: {viewport:?}\npainted: {painted:#?}\nleaves: {leaves:#?}"
            ),
        )?;
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
    ensure!(
        path.is_file(),
        "Missing approved baseline {}. Review artifacts and explicitly use --update-baselines to approve them.",
        path.display()
    );
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
        save_screenshot(&difference, diff_path)?;
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

#[cfg(test)]
mod screenshot_tests {
    use super::*;

    #[test]
    fn preview_raster_probe_cannot_count_body_or_controls_outside_its_box() {
        let mut screenshot = RgbaImage::from_pixel(32, 32, Rgba([30, 30, 30, 255]));
        for y in 0..8 {
            for x in 0..8 {
                screenshot.put_pixel(x, y, Rgba([209, 62, 31, 255]));
            }
        }
        let preview = gpui::Bounds::new(point(px(8.), px(8.)), size(px(4.), px(4.)));
        assert_eq!(orange_raster_pixels(&screenshot, preview, 2.), 0);
        screenshot.put_pixel(20, 20, Rgba([209, 62, 31, 255]));
        assert_eq!(orange_raster_pixels(&screenshot, preview, 2.), 1);
        screenshot.put_pixel(21, 20, Rgba([209, 209, 209, 255]));
        assert_eq!(orange_raster_pixels(&screenshot, preview, 2.), 1);
    }

    #[test]
    fn journey_subset_cannot_silently_replace_a_complete_baseline_gate() {
        let options = Options::parse(
            ["--journeys", "mouse_", "--geometry-only"]
                .map(str::to_owned)
                .into_iter(),
        )
        .unwrap();
        assert_eq!(options.journeys.as_deref(), Some("mouse_"));
        for args in [
            vec!["--journeys", "mouse_", "--baseline", "approved"],
            vec!["--journeys", "mouse_", "--filter", "paragraph"],
            vec!["--journeys", "mouse_", "--notepad-only"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn image_subset_never_reads_or_updates_golden_baselines() {
        assert!(
            Options::parse(
                ["--images-only", "--geometry-only"]
                    .into_iter()
                    .map(str::to_owned)
            )
            .unwrap()
            .images_only
        );
        for args in [
            vec!["--images-only", "--baseline", "approved"],
            vec!["--images-only", "--notepad-only"],
            vec!["--images-only", "--concurrent-only"],
            vec!["--images-only", "--journeys", "pointer_"],
            vec!["--images-only", "--filter", "paragraph"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn desktop_subsets_never_mutate_baselines_or_replace_other_gates() {
        for flag in ["--open-path-only", "--locale-only"] {
            let options =
                Options::parse([flag, "--geometry-only"].into_iter().map(str::to_owned)).unwrap();
            assert!(options.open_path_only || options.locale_only);
            for conflicting in [
                vec![flag, "--baseline", "approved"],
                vec![flag, "--images-only"],
                vec![flag, "--notepad-only"],
                vec![flag, "--concurrent-only"],
                vec![flag, "--journeys", "find_"],
                vec![flag, "--record-frames"],
                vec![flag, "--filter", "paragraph"],
                vec!["--open-path-only", "--locale-only"],
            ] {
                assert!(Options::parse(conflicting.into_iter().map(str::to_owned)).is_err());
            }
        }
    }

    #[test]
    fn notepad_subset_is_isolated_from_baseline_mutation_and_other_subsets() {
        let options = Options::parse(
            ["--notepad-only", "--geometry-only"]
                .map(str::to_owned)
                .into_iter(),
        )
        .unwrap();
        assert!(options.notepad_only && options.geometry_only);
        for args in [
            vec!["--notepad-only", "--baseline", "approved"],
            vec!["--notepad-only", "--record-frames"],
            vec!["--notepad-only", "--concurrent-only"],
        ] {
            assert!(Options::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn concurrent_subset_is_explicit_and_cannot_update_golden_references() {
        assert!(!Options::parse(std::iter::empty()).unwrap().concurrent_only);
        let options = Options::parse(
            ["--concurrent-only", "--geometry-only"]
                .map(str::to_owned)
                .into_iter(),
        )
        .unwrap();
        assert!(options.concurrent_only && options.geometry_only);
        assert!(Options::parse(
            ["--concurrent-only", "--baseline", "approved"]
                .map(str::to_owned)
                .into_iter()
        )
        .is_err());
        assert!(Options::parse(
            ["--concurrent-only", "--record-frames"]
                .map(str::to_owned)
                .into_iter()
        )
        .is_err());
    }

    #[test]
    fn fast_png_capture_preserves_every_rgba_pixel() {
        let image = RgbaImage::from_fn(17, 9, |x, y| {
            Rgba([
                (x * 13) as u8,
                (y * 23) as u8,
                ((x + y) * 7) as u8,
                (x * y) as u8,
            ])
        });
        let mut encoded = Vec::new();
        encode_screenshot(&image, &mut encoded).unwrap();
        assert_eq!(
            image::load_from_memory(&encoded).unwrap().into_rgba8(),
            image
        );
    }
}
