// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::{Path, PathBuf};

use crate::config::is_markdown;

/// Returns true when the path looks like a raster/vector image file.
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "svg"
                    | "bmp"
                    | "ico"
                    | "heic"
                    | "tif"
                    | "tiff"
            )
        })
        .unwrap_or(false)
}

/// Classify dropped paths for workspace-level handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropIntent {
    OpenWorkspace(PathBuf),
    OpenDocuments(Vec<PathBuf>),
    InsertImages(Vec<PathBuf>),
    Ignored,
}

/// Decide how to handle a drop on the main window chrome.
pub fn classify_window_drop(paths: &[PathBuf]) -> DropIntent {
    if paths.is_empty() {
        return DropIntent::Ignored;
    }

    if paths.len() == 1 && paths[0].is_dir() {
        return DropIntent::OpenWorkspace(paths[0].clone());
    }

    let markdown: Vec<_> = paths.iter().filter(|p| is_markdown(p)).cloned().collect();
    if !markdown.is_empty() {
        return DropIntent::OpenDocuments(markdown);
    }

    DropIntent::Ignored
}

/// Decide how to handle a drop on the editor surface.
pub fn classify_editor_drop(paths: &[PathBuf]) -> DropIntent {
    if paths.is_empty() {
        return DropIntent::Ignored;
    }

    let images: Vec<_> = paths.iter().filter(|p| is_image(p)).cloned().collect();
    if !images.is_empty() {
        return DropIntent::InsertImages(images);
    }

    classify_window_drop(paths)
}

/// Copy an image beside the document (or workspace) and return a markdown reference.
pub fn markdown_image_reference(image_path: &Path, document_path: Option<&Path>) -> Option<String> {
    let filename = image_path.file_name()?.to_os_string();
    let base = document_path
        .and_then(|p| p.parent())
        .or_else(|| image_path.parent())?;
    let assets_dir = base.join("assets");
    std::fs::create_dir_all(&assets_dir).ok()?;
    let dest = assets_dir.join(&filename);
    if image_path != dest {
        std::fs::copy(image_path, &dest).ok()?;
    }
    let rel = format!("assets/{}", filename.to_string_lossy());
    Some(format!(
        "![{}]({})",
        filename.to_string_lossy(),
        rel.replace(' ', "%20")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_single_folder_as_workspace() {
        let path = std::env::temp_dir();
        assert_eq!(
            classify_window_drop(std::slice::from_ref(&path)),
            DropIntent::OpenWorkspace(path)
        );
    }

    #[test]
    fn classify_markdown_files() {
        let paths = vec![
            PathBuf::from("/tmp/readme.md"),
            PathBuf::from("/tmp/photo.png"),
        ];
        assert_eq!(
            classify_window_drop(&paths),
            DropIntent::OpenDocuments(vec![PathBuf::from("/tmp/readme.md")])
        );
    }

    #[test]
    fn classify_editor_images() {
        let paths = vec![PathBuf::from("/tmp/photo.png")];
        assert_eq!(
            classify_editor_drop(&paths),
            DropIntent::InsertImages(paths)
        );
    }

    #[test]
    fn is_image_detects_common_formats() {
        assert!(is_image(Path::new("x.PNG")));
        assert!(is_image(Path::new("x.heic")));
        assert!(is_image(Path::new("x.svg")));
        assert!(!is_image(Path::new("x.md")));
        assert!(!is_image(Path::new("x.rs")));
    }

    #[test]
    fn empty_drop_is_ignored() {
        assert_eq!(classify_window_drop(&[]), DropIntent::Ignored);
        assert_eq!(classify_editor_drop(&[]), DropIntent::Ignored);
    }

    #[test]
    fn window_ignores_images_and_unknown_files() {
        let paths = vec![
            PathBuf::from("/tmp/photo.png"),
            PathBuf::from("/tmp/notes.pdf"),
        ];
        assert_eq!(classify_window_drop(&paths), DropIntent::Ignored);
    }

    #[test]
    fn editor_prefers_images_over_markdown() {
        let paths = vec![
            PathBuf::from("/tmp/readme.md"),
            PathBuf::from("/tmp/photo.png"),
        ];
        assert_eq!(
            classify_editor_drop(&paths),
            DropIntent::InsertImages(vec![PathBuf::from("/tmp/photo.png")])
        );
    }

    #[test]
    fn editor_falls_back_to_folder_and_markdown() {
        let folder = std::env::temp_dir();
        assert_eq!(
            classify_editor_drop(std::slice::from_ref(&folder)),
            DropIntent::OpenWorkspace(folder)
        );
        assert_eq!(
            classify_editor_drop(&[PathBuf::from("/tmp/notes.markdown")]),
            DropIntent::OpenDocuments(vec![PathBuf::from("/tmp/notes.markdown")])
        );
    }

    #[test]
    fn markdown_image_reference_copies_into_assets() {
        let dir = std::env::temp_dir().join(format!(
            "markrust-drop-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("note.md");
        std::fs::write(&doc, "# hi\n").unwrap();
        let image = dir.join("photo.png");
        std::fs::write(&image, b"fake-png").unwrap();

        let snippet = markdown_image_reference(&image, Some(&doc)).unwrap();
        assert_eq!(snippet, "![photo.png](assets/photo.png)");
        assert!(dir.join("assets/photo.png").exists());
        let copied = std::fs::read(dir.join("assets/photo.png")).unwrap();
        assert_eq!(copied, b"fake-png");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
