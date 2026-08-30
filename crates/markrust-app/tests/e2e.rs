// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless user-journey tests. These never open a GPUI window.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use markrust_app::{
    classify_editor_drop, classify_window_drop, markdown_image_reference, reload_decision,
    DropIntent, DropTarget, ExternalChangeAction, HeadlessWorkspace, SessionError,
    WorkspaceCommand,
};
use markrust_core::markdown_to_html_gfm;
use markrust_editor::{EditorCommand, VisibilityState, WrapKind};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn unique_temp(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("markrust-{prefix}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn open_file_extracts_outline_and_jumps_to_heading() {
    let mut workspace = HeadlessWorkspace::new();
    workspace
        .apply(WorkspaceCommand::OpenFile(fixture("showcase.md")))
        .unwrap();
    let headings = workspace.active_mut().unwrap().editor.outline();
    assert!(
        headings
            .iter()
            .any(|(_, _, title)| title.contains("MarkRust Editor Showcase")),
        "{headings:?}"
    );
    let lists = headings
        .iter()
        .find(|(_, _, title)| title == "Lists")
        .cloned();
    let (offset, _, _) = lists.expect("Lists heading");
    workspace
        .apply(WorkspaceCommand::JumpToHeading { offset })
        .unwrap();
    assert_eq!(workspace.active().unwrap().editor.cursor_offset(), offset);
}

#[test]
fn bold_wrap_masks_when_caret_is_outside() {
    let mut workspace = HeadlessWorkspace::new();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
            "hello **x** world".into(),
        )))
        .unwrap();
    let content = workspace.active().unwrap().editor.content();
    let bold = content.find("**x**").unwrap();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::JumpTo(bold + 3)))
        .unwrap();
    let inside = workspace.active_mut().unwrap().editor.visibility();
    assert!(inside.contains(&VisibilityState::Visible));
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::JumpTo(0)))
        .unwrap();
    let outside = workspace.active_mut().unwrap().editor.visibility();
    assert!(outside.contains(&VisibilityState::Masked));
    assert!(!outside.contains(&VisibilityState::Visible));
}

#[test]
fn wrap_bold_command_then_mask_outside() {
    let mut workspace = HeadlessWorkspace::new();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
            "hello world".into(),
        )))
        .unwrap();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::SetSelection {
            start: 0,
            end: 5,
        }))
        .unwrap();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::Wrap(
            WrapKind::Bold,
        )))
        .unwrap();
    assert_eq!(
        workspace.active().unwrap().editor.content(),
        "**hello** world"
    );
    let inside = workspace.active_mut().unwrap().editor.visibility();
    assert!(inside.contains(&VisibilityState::Visible));
    let end = workspace.active().unwrap().editor.content().len();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::JumpTo(end)))
        .unwrap();
    let outside = workspace.active_mut().unwrap().editor.visibility();
    assert!(outside.contains(&VisibilityState::Masked));
    assert!(!outside.contains(&VisibilityState::Visible));
}

#[test]
fn task_table_fence_frontmatter_survive_save_roundtrip() {
    let dir = unique_temp("roundtrip");
    let dest = dir.join("copy.md");
    std::fs::copy(fixture("showcase.md"), &dest).unwrap();

    let mut workspace = HeadlessWorkspace::new();
    workspace
        .apply(WorkspaceCommand::OpenFile(dest.clone()))
        .unwrap();
    let original = workspace.active().unwrap().editor.content();
    assert!(original.contains("---"));
    assert!(original.contains("- [x]"));
    assert!(original.contains("| Feature"));
    assert!(original.contains("```rust"));

    workspace.apply(WorkspaceCommand::Save).unwrap();
    let reloaded = std::fs::read_to_string(&dest).unwrap();
    assert_eq!(original, reloaded);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn export_html_contains_table_and_task_list() {
    let dir = unique_temp("export");
    let src = dir.join("doc.md");
    std::fs::copy(fixture("showcase.md"), &src).unwrap();
    let out = dir.join("doc.html");

    let mut workspace = HeadlessWorkspace::new();
    workspace.apply(WorkspaceCommand::OpenFile(src)).unwrap();
    workspace
        .apply(WorkspaceCommand::ExportHtml {
            output: Some(out.clone()),
        })
        .unwrap();
    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("<table>"), "{html}");
    assert!(
        html.contains("checkbox")
            || html.contains("task-list")
            || html.contains("type=\"checkbox\""),
        "{html}"
    );

    let lib_html = markdown_to_html_gfm(&std::fs::read_to_string(fixture("showcase.md")).unwrap());
    assert!(lib_html.contains("<table>"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn drop_classifier_folder_md_and_image_insert() {
    let dir = unique_temp("drop");
    let md = dir.join("note.md");
    std::fs::write(&md, "# Note\n").unwrap();
    let image = dir.join("photo.png");
    std::fs::write(&image, b"fake-png").unwrap();

    assert_eq!(
        classify_window_drop(std::slice::from_ref(&dir)),
        DropIntent::OpenWorkspace(dir.clone())
    );
    assert_eq!(
        classify_window_drop(std::slice::from_ref(&md)),
        DropIntent::OpenDocuments(vec![md.clone()])
    );
    assert_eq!(
        classify_editor_drop(std::slice::from_ref(&image)),
        DropIntent::InsertImages(vec![image.clone()])
    );

    let mut workspace = HeadlessWorkspace::new();
    workspace
        .apply(WorkspaceCommand::SaveAs(md.clone()))
        .unwrap();
    workspace
        .apply(WorkspaceCommand::DropFiles {
            paths: vec![image.clone()],
            target: DropTarget::Editor,
        })
        .unwrap();
    let content = workspace.active().unwrap().editor.content();
    assert!(
        content.contains("![photo.png](assets/photo.png)"),
        "{content}"
    );
    assert!(dir.join("assets/photo.png").exists());
    let snippet = markdown_image_reference(&image, Some(&md)).unwrap();
    assert_eq!(snippet, "![photo.png](assets/photo.png)");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn autosave_debounce_uses_fake_clock() {
    let dir = unique_temp("autosave");
    let path = dir.join("note.md");
    std::fs::write(&path, "start\n").unwrap();

    let mut workspace = HeadlessWorkspace::with_autosave_ms(1000);
    workspace
        .apply(WorkspaceCommand::OpenFile(path.clone()))
        .unwrap();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
            " edited".into(),
        )))
        .unwrap();
    assert!(workspace.active().unwrap().editor.document().dirty);
    workspace
        .apply(WorkspaceCommand::AdvanceTime { millis: 999 })
        .unwrap();
    assert!(workspace.active().unwrap().editor.document().dirty);
    workspace
        .apply(WorkspaceCommand::AdvanceTime { millis: 1 })
        .unwrap();
    assert!(!workspace.active().unwrap().editor.document().dirty);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("edited"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_watcher_reload_decision_and_reload_tab() {
    let dir = unique_temp("watch");
    let path = dir.join("note.md");
    std::fs::write(&path, "v1\n").unwrap();

    let mut workspace = HeadlessWorkspace::new();
    workspace
        .apply(WorkspaceCommand::OpenFile(path.clone()))
        .unwrap();
    assert_eq!(
        reload_decision(false, Some(&path), &path),
        ExternalChangeAction::PromptReload
    );

    std::fs::write(&path, "v2\n").unwrap();
    workspace
        .apply(WorkspaceCommand::ExternalFileChange(path.clone()))
        .unwrap();
    assert!(workspace.pending_external_change.is_some());
    let index = workspace.active_tab;
    workspace.apply(WorkspaceCommand::ReloadTab(index)).unwrap();
    assert_eq!(workspace.active().unwrap().editor.content(), "v2\n");

    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
            "dirty".into(),
        )))
        .unwrap();
    workspace.pending_external_change = None;
    workspace
        .apply(WorkspaceCommand::ExternalFileChange(path.clone()))
        .unwrap();
    assert!(workspace.pending_external_change.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dirty_tab_merges_disjoint_external_edits() {
    let dir = unique_temp("merge");
    let path = dir.join("note.md");
    std::fs::write(&path, "aaa\nbbb\nccc\n").unwrap();

    let mut workspace = HeadlessWorkspace::new();
    workspace
        .apply(WorkspaceCommand::OpenFile(path.clone()))
        .unwrap();
    let content = workspace.active().unwrap().editor.content();
    let at = content.find("bbb").unwrap();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::SetSelection {
            start: at,
            end: at + 3,
        }))
        .unwrap();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
            "BBB".into(),
        )))
        .unwrap();
    assert!(workspace.active().unwrap().editor.document().dirty);
    std::fs::write(&path, "aaa\nbbb\nCCC\n").unwrap();
    workspace
        .apply(WorkspaceCommand::ExternalFileChange(path.clone()))
        .unwrap();
    assert_eq!(
        workspace.active().unwrap().editor.content(),
        "aaa\nBBB\nCCC\n"
    );
    assert!(workspace.active().unwrap().editor.document().dirty);
    assert!(workspace.pending_external_change.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multi_tab_switch_dirty_and_close() {
    let dir = unique_temp("tabs");
    let a = dir.join("a.md");
    let b = dir.join("b.md");
    std::fs::write(&a, "aaa\n").unwrap();
    std::fs::write(&b, "bbb\n").unwrap();

    let mut workspace = HeadlessWorkspace::new();
    workspace.apply(WorkspaceCommand::OpenFile(a)).unwrap();
    workspace.apply(WorkspaceCommand::OpenFile(b)).unwrap();
    // Opening a real file dismisses the pristine Untitled placeholder tab.
    assert_eq!(workspace.tabs().len(), 2);
    workspace.apply(WorkspaceCommand::SwitchTab(0)).unwrap();
    workspace
        .apply(WorkspaceCommand::Editor(EditorCommand::InsertText(
            "x".into(),
        )))
        .unwrap();
    assert!(workspace.tabs()[0].editor.document().dirty);
    assert!(!workspace.tabs()[1].editor.document().dirty);
    workspace.apply(WorkspaceCommand::CloseTab).unwrap();
    assert_eq!(workspace.tabs().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn untitled_cannot_save_without_path() {
    let mut workspace = HeadlessWorkspace::new();
    let err = workspace.apply(WorkspaceCommand::Save).unwrap_err();
    assert!(matches!(err, SessionError::UntitledHasNoPath));
}

#[test]
fn open_folder_lists_fixture_markdown() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut workspace = HeadlessWorkspace::new();
    workspace
        .apply(WorkspaceCommand::OpenFolder(fixtures))
        .unwrap();
    let files = workspace.list_files();
    assert!(
        files
            .iter()
            .any(|path| path.file_name() == Some(std::ffi::OsStr::new("showcase.md"))),
        "{files:?}"
    );
}

#[test]
fn should_skip_dir_ignores_target() {
    let dir = unique_temp("skip");
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::write(dir.join("target/hidden.md"), "x").unwrap();
    std::fs::write(dir.join("ok.md"), "x").unwrap();
    let files = markrust_app::list_markdown_files(&dir);
    assert_eq!(files, vec![dir.join("ok.md")]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_large_markdown_file_does_not_hang() {
    let dir = unique_temp("open-large");
    let path = dir.join("large.md");
    let mut body = String::with_capacity(128 * 1024);
    for i in 0..1_500 {
        body.push_str("# H");
        body.push_str(&i.to_string());
        body.push_str("\n\npara **x**\n\n");
    }
    std::fs::write(&path, &body).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut workspace = HeadlessWorkspace::new();
        workspace
            .apply(WorkspaceCommand::OpenFile(path))
            .expect("open");
        let elapsed = started.elapsed();
        let content = workspace.active().unwrap().editor.content();
        let _ = tx.send((elapsed, content.len()));
    });
    let (elapsed, len) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("opening a markdown file hung");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "OpenFile blocked for {elapsed:?}"
    );
    assert!(len > 10_000, "fixture too small: {len}");
    let _ = std::fs::remove_dir_all(&dir);
}
