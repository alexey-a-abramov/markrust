// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native desktop shell using egui. GPUI currently fails to rasterize glyphs on
//! recent macOS; egui embeds its own fonts so labels and the editor are visible.

use std::path::PathBuf;

use eframe::egui::{
    self, Color32, FontFamily, FontId, Frame, Key, Layout, Margin, RichText, TextEdit, TextStyle,
    ViewportBuilder, Visuals,
};
use eframe::{App, CreationContext, NativeOptions};
use markrust_editor::{outline_headings, EditorCommand};

use crate::config::{AppConfig, RecentWorkspaces};
use crate::session::{DropTarget, HeadlessWorkspace, SessionError, WorkspaceCommand};

const ACCENT: Color32 = Color32::from_rgb(0xc2, 0x41, 0x0c);
const PAPER_DARK: Color32 = Color32::from_rgb(0x1c, 0x19, 0x17);
const CHROME_DARK: Color32 = Color32::from_rgb(0x29, 0x25, 0x24);
const SIDEBAR_DARK: Color32 = Color32::from_rgb(0x24, 0x1f, 0x1c);
const TEXT_DARK: Color32 = Color32::from_rgb(0xf5, 0xf0, 0xe8);
const MUTED_DARK: Color32 = Color32::from_rgb(0xa8, 0xa2, 0x9e);

const PAPER_LIGHT: Color32 = Color32::from_rgb(0xff, 0xfc, 0xf7);
const CHROME_LIGHT: Color32 = Color32::from_rgb(0xf3, 0xee, 0xe7);
const SIDEBAR_LIGHT: Color32 = Color32::from_rgb(0xee, 0xe8, 0xe0);
const TEXT_LIGHT: Color32 = Color32::from_rgb(0x1c, 0x19, 0x17);
const MUTED_LIGHT: Color32 = Color32::from_rgb(0x78, 0x71, 0x6c);

pub fn run(open_path: Option<PathBuf>) {
    let options = NativeOptions {
        viewport: ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("MarkRust"),
        ..Default::default()
    };
    if let Err(error) = eframe::run_native(
        "MarkRust",
        options,
        Box::new(move |cc| Ok(Box::new(MarkRustNative::new(cc, open_path)))),
    ) {
        eprintln!("MarkRust failed to start: {error}");
        std::process::exit(1);
    }
}

struct Palette {
    paper: Color32,
    chrome: Color32,
    sidebar: Color32,
    text: Color32,
    muted: Color32,
    accent: Color32,
}

impl Palette {
    fn dark() -> Self {
        Self {
            paper: PAPER_DARK,
            chrome: CHROME_DARK,
            sidebar: SIDEBAR_DARK,
            text: TEXT_DARK,
            muted: MUTED_DARK,
            accent: ACCENT,
        }
    }

    fn light() -> Self {
        Self {
            paper: PAPER_LIGHT,
            chrome: CHROME_LIGHT,
            sidebar: SIDEBAR_LIGHT,
            text: TEXT_LIGHT,
            muted: MUTED_LIGHT,
            accent: Color32::from_rgb(0x9a, 0x34, 0x12),
        }
    }
}

struct MarkRustNative {
    workspace: HeadlessWorkspace,
    config: AppConfig,
    status: String,
}

impl MarkRustNative {
    fn new(_cc: &CreationContext<'_>, open_path: Option<PathBuf>) -> Self {
        let config = AppConfig::load();
        let mut workspace = HeadlessWorkspace::with_autosave_ms(config.autosave_ms);
        workspace.theme = config.theme;
        if let Some(path) = open_path {
            if let Err(error) = workspace.apply(WorkspaceCommand::OpenLaunchPath(path)) {
                eprintln!("Failed to open path: {error}");
            }
        }
        Self {
            workspace,
            config,
            status: "Ready".into(),
        }
    }

    fn palette(&self) -> Palette {
        match self.workspace.theme {
            crate::config::ThemeChoice::Dark => Palette::dark(),
            crate::config::ThemeChoice::Light => Palette::light(),
        }
    }

    fn apply_visuals(&self, ctx: &egui::Context) {
        let palette = self.palette();
        let mut visuals = match self.workspace.theme {
            crate::config::ThemeChoice::Dark => Visuals::dark(),
            crate::config::ThemeChoice::Light => Visuals::light(),
        };
        visuals.panel_fill = palette.chrome;
        visuals.window_fill = palette.paper;
        visuals.extreme_bg_color = palette.paper;
        visuals.override_text_color = Some(palette.text);
        visuals.selection.bg_fill = palette.accent.gamma_multiply(0.45);
        visuals.widgets.inactive.bg_fill = palette.sidebar;
        visuals.widgets.hovered.bg_fill = palette.accent.gamma_multiply(0.35);
        visuals.widgets.active.bg_fill = palette.accent;
        ctx.set_visuals(visuals);
        ctx.style_mut(|style| {
            style
                .text_styles
                .insert(TextStyle::Body, FontId::new(16.0, FontFamily::Proportional));
            style.text_styles.insert(
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            );
            style.text_styles.insert(
                TextStyle::Heading,
                FontId::new(22.0, FontFamily::Proportional),
            );
            style.text_styles.insert(
                TextStyle::Small,
                FontId::new(12.0, FontFamily::Proportional),
            );
            style.text_styles.insert(
                TextStyle::Monospace,
                FontId::new(15.0, FontFamily::Monospace),
            );
        });
    }

    fn dispatch(&mut self, command: WorkspaceCommand) {
        match self.workspace.apply(command) {
            Ok(_) => self.status = "Ready".into(),
            Err(SessionError::UntitledHasNoPath) => {
                self.save_as();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn open_file_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Markdown", &["md", "markdown", "txt"])
            .pick_file()
        {
            self.remember_workspace(&path);
            self.dispatch(WorkspaceCommand::OpenFile(path));
        }
    }

    fn remember_recent(&self, path: PathBuf) {
        let mut recent = RecentWorkspaces::load();
        recent.push(path);
        let _ = recent.save();
    }

    fn open_folder_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.remember_recent(path.clone());
            self.dispatch(WorkspaceCommand::OpenFolder(path));
        }
    }

    fn remember_workspace(&mut self, file: &std::path::Path) {
        if self.workspace.root.is_none() {
            if let Some(parent) = file.parent() {
                self.dispatch(WorkspaceCommand::OpenFolder(parent.to_path_buf()));
            }
        }
    }

    fn save_as(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Markdown", &["md", "markdown"])
            .set_file_name("untitled.md")
            .save_file()
        {
            self.dispatch(WorkspaceCommand::SaveAs(path));
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let mut commands = Vec::new();
        let mut open_file = false;
        let mut open_folder = false;
        ctx.input(|input| {
            let cmd = input.modifiers.command;
            if cmd && input.key_pressed(Key::S) {
                commands.push(WorkspaceCommand::Save);
            }
            if cmd && input.key_pressed(Key::N) {
                commands.push(WorkspaceCommand::NewDocument);
            }
            if cmd && input.key_pressed(Key::W) {
                commands.push(WorkspaceCommand::CloseTab);
            }
            if cmd && input.key_pressed(Key::Z) {
                if input.modifiers.shift {
                    commands.push(WorkspaceCommand::Editor(EditorCommand::Redo));
                } else {
                    commands.push(WorkspaceCommand::Editor(EditorCommand::Undo));
                }
            }
            if cmd && input.key_pressed(Key::O) {
                if input.modifiers.shift {
                    open_folder = true;
                } else {
                    open_file = true;
                }
            }
            if cmd && input.modifiers.shift && input.key_pressed(Key::T) {
                commands.push(WorkspaceCommand::ToggleTheme);
            }
        });
        for command in commands {
            if matches!(command, WorkspaceCommand::ToggleTheme) {
                self.dispatch(command);
                self.config.theme = self.workspace.theme;
                let _ = self.config.save();
            } else {
                self.dispatch(command);
            }
        }
        if open_folder {
            self.open_folder_dialog();
        } else if open_file {
            self.open_file_dialog();
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let paths: Vec<PathBuf> = ctx.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if !paths.is_empty() {
            self.dispatch(WorkspaceCommand::DropFiles {
                paths,
                target: DropTarget::Window,
            });
        }
    }

    fn tick_autosave(&mut self) {
        let _ = self
            .workspace
            .apply(WorkspaceCommand::AdvanceTime { millis: 16 });
    }
}

impl App for MarkRustNative {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.apply_visuals(ctx);
        self.handle_shortcuts(ctx);
        self.handle_drops(ctx);
        self.tick_autosave();

        let palette = self.palette();
        let button = |label: &str| RichText::new(label).color(palette.text).size(14.0);

        egui::TopBottomPanel::top("toolbar")
            .frame(
                Frame::new()
                    .fill(palette.chrome)
                    .inner_margin(Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("MarkRust").color(palette.text).strong());
                    ui.add_space(12.0);
                    if ui.button(button("New")).clicked() {
                        self.dispatch(WorkspaceCommand::NewDocument);
                    }
                    if ui.button(button("Open File")).clicked() {
                        self.open_file_dialog();
                    }
                    if ui.button(button("Open Folder")).clicked() {
                        self.open_folder_dialog();
                    }
                    if ui.button(button("Save")).clicked() {
                        self.dispatch(WorkspaceCommand::Save);
                    }
                    if ui.button(button("Theme")).clicked() {
                        self.dispatch(WorkspaceCommand::ToggleTheme);
                        self.config.theme = self.workspace.theme;
                        let _ = self.config.save();
                    }
                });
            });

        egui::TopBottomPanel::bottom("status")
            .frame(
                Frame::new()
                    .fill(palette.chrome)
                    .inner_margin(Margin::symmetric(12, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (path, words, dirty, line) = self
                        .workspace
                        .active()
                        .map(|tab| {
                            let doc = tab.editor.document();
                            (
                                doc.path
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "Untitled".into()),
                                tab.editor.word_count(),
                                doc.dirty,
                                tab.editor.cursor_offset(),
                            )
                        })
                        .unwrap_or_else(|| ("—".into(), 0, false, 0));
                    ui.label(RichText::new(path).color(palette.muted).size(12.0));
                    ui.separator();
                    ui.label(
                        RichText::new(if dirty { "Unsaved" } else { "Saved" })
                            .color(if dirty { palette.accent } else { palette.muted })
                            .size(12.0),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(format!("{words} words · byte {line}"))
                            .color(palette.muted)
                            .size(12.0),
                    );
                    ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(&self.status).color(palette.muted).size(12.0));
                    });
                });
            });

        egui::SidePanel::left("files")
            .resizable(true)
            .default_width(240.0)
            .frame(Frame::new().fill(palette.sidebar).inner_margin(Margin::symmetric(10, 10)))
            .show(ctx, |ui| {
                ui.label(RichText::new("Files").color(palette.muted).size(12.0).strong());
                ui.add_space(8.0);
                let files = self.workspace.list_files();
                if files.is_empty() {
                    ui.label(
                        RichText::new("Welcome to MarkRust")
                            .color(palette.text)
                            .size(18.0)
                            .strong(),
                    );
                    ui.label(
                        RichText::new("Open a folder or file to get started. You can also drop files onto this window.")
                            .color(palette.muted)
                            .size(13.0),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::Button::new(RichText::new("Open Folder").color(Color32::WHITE)).fill(palette.accent))
                            .clicked()
                        {
                            self.open_folder_dialog();
                        }
                        if ui.button(RichText::new("Open File").color(palette.text)).clicked() {
                            self.open_file_dialog();
                        }
                    });
                    let recent = RecentWorkspaces::load();
                    if !recent.workspaces.is_empty() {
                        ui.add_space(12.0);
                        ui.label(RichText::new("Recent").color(palette.muted).size(12.0));
                        for path in recent.workspaces {
                            let label = path.display().to_string();
                            if ui
                                .add(egui::Button::new(RichText::new(label).color(palette.text).size(13.0)).fill(Color32::TRANSPARENT))
                                .clicked()
                            {
                                if path.is_dir() {
                                    self.dispatch(WorkspaceCommand::OpenFolder(path));
                                } else {
                                    self.dispatch(WorkspaceCommand::OpenFile(path));
                                }
                            }
                        }
                    }
                } else {
                    let root = self.workspace.root.clone();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for path in files {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            let relative = root
                                .as_ref()
                                .and_then(|root| path.strip_prefix(root).ok())
                                .map(|p| p.display().to_string())
                                .unwrap_or(name);
                            let selected = self.workspace.active().is_some_and(|tab| {
                                tab.editor.document().path.as_deref() == Some(path.as_path())
                            });
                            let fill = if selected {
                                palette.accent.gamma_multiply(0.35)
                            } else {
                                Color32::TRANSPARENT
                            };
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(relative).color(palette.text).size(13.0),
                                    )
                                    .fill(fill),
                                )
                                .clicked()
                            {
                                self.dispatch(WorkspaceCommand::OpenFile(path));
                            }
                        }
                    });
                }
            });

        egui::SidePanel::right("outline")
            .resizable(true)
            .default_width(220.0)
            .frame(
                Frame::new()
                    .fill(palette.sidebar)
                    .inner_margin(Margin::symmetric(10, 10)),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new("Outline")
                        .color(palette.muted)
                        .size(12.0)
                        .strong(),
                );
                ui.add_space(8.0);
                let headings = self.workspace.active_mut().map(|tab| {
                    let _ = tab.editor.visibility();
                    outline_headings(&tab.editor.document().syntax_spans, &tab.editor.content())
                });
                match headings {
                    Some(items) if !items.is_empty() => {
                        for (offset, level, title) in items {
                            let indent = ((level.saturating_sub(1)) as f32) * 12.0;
                            ui.horizontal(|ui| {
                                ui.add_space(indent);
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(title).color(palette.text).size(13.0),
                                        )
                                        .fill(Color32::TRANSPARENT),
                                    )
                                    .clicked()
                                {
                                    self.dispatch(WorkspaceCommand::JumpToHeading { offset });
                                }
                            });
                        }
                    }
                    _ => {
                        ui.label(
                            RichText::new("Headings in this document will appear here.")
                                .color(palette.muted)
                                .size(13.0),
                        );
                    }
                }
            });

        egui::TopBottomPanel::top("tabs")
            .frame(
                Frame::new()
                    .fill(palette.chrome)
                    .inner_margin(Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let titles: Vec<(usize, String, bool)> = self
                        .workspace
                        .tabs()
                        .iter()
                        .enumerate()
                        .map(|(index, tab)| {
                            let dirty = tab.editor.document().dirty;
                            let title = if dirty {
                                format!("• {}", tab.title)
                            } else {
                                tab.title.clone()
                            };
                            (index, title, index == self.workspace.active_tab)
                        })
                        .collect();
                    for (index, title, active) in titles {
                        let fill = if active {
                            palette.paper
                        } else {
                            Color32::TRANSPARENT
                        };
                        let response = ui.add(
                            egui::Button::new(RichText::new(title).color(palette.text).size(13.0))
                                .fill(fill),
                        );
                        if response.clicked() {
                            self.dispatch(WorkspaceCommand::SwitchTab(index));
                        }
                        if response.middle_clicked() {
                            self.dispatch(WorkspaceCommand::SwitchTab(index));
                            self.dispatch(WorkspaceCommand::CloseTab);
                        }
                    }
                });
            });

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(palette.paper)
                    .inner_margin(Margin::symmetric(24, 20)),
            )
            .show(ctx, |ui| {
                let mut draft = self
                    .workspace
                    .active()
                    .map(|tab| tab.editor.content())
                    .unwrap_or_default();
                let editor = TextEdit::multiline(&mut draft)
                    .font(FontId::new(17.0, FontFamily::Proportional))
                    .desired_width(f32::INFINITY)
                    .desired_rows(40)
                    .frame(false)
                    .text_color(palette.text);
                let output = editor.show(ui);
                if output.response.changed() {
                    if let Some(tab) = self.workspace.active_mut() {
                        tab.editor.set_content_from_ui(&draft);
                    }
                }
            });
    }
}
