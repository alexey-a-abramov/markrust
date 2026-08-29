// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use comrak::{markdown_to_html, Options};

/// Render Markdown source to an HTML fragment using GFM extensions.
pub fn markdown_to_html_gfm(source: &str) -> String {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.tagfilter = true;
    options.extension.front_matter_delimiter = Some("---".into());
    options.render.unsafe_ = true;
    markdown_to_html(source, &options)
}

/// Write Markdown source to an HTML file at `output`.
pub fn write_markdown_to_html_file(source: &str, output: &Path) -> io::Result<()> {
    let html = markdown_to_html_gfm(source);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, html)
}

/// Export Markdown source to HTML, inferring the output path from `input_path` when needed.
pub fn export_content_to_html(
    source: &str,
    input_path: Option<&Path>,
    output: Option<&Path>,
) -> io::Result<PathBuf> {
    let out_path = output
        .map(PathBuf::from)
        .or_else(|| input_path.map(|p| p.with_extension("html")))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "output path required when source has no file path",
            )
        })?;
    write_markdown_to_html_file(source, &out_path)?;
    Ok(out_path)
}

/// Read a Markdown file and write HTML to `output` (or `<stem>.html` beside the source).
pub fn export_file_to_html(input: &Path, output: Option<&Path>) -> io::Result<PathBuf> {
    let source = fs::read_to_string(input)?;
    export_content_to_html(&source, Some(input), output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_tables_and_task_lists() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n\n- [x] done\n";
        let html = markdown_to_html_gfm(md);
        assert!(html.contains("<table>"));
        assert!(html.contains("checkbox") || html.contains("[x]") || html.contains("checked"));
    }

    #[test]
    fn exports_frontmatter_without_rendering_fence() {
        let md = "---\ntitle: Test\n---\n\n# Body\n";
        let html = markdown_to_html_gfm(md);
        assert!(html.contains("<h1>Body</h1>") || html.contains("Body</h1>"));
        assert!(!html.contains("title: Test"));
    }

    #[test]
    fn exports_strikethrough_and_autolink() {
        let html = markdown_to_html_gfm("~~gone~~ and https://example.com");
        assert!(
            html.contains("<del>") && html.contains("gone"),
            "strikethrough html: {html}"
        );
        assert!(
            html.contains("href=\"https://example.com\""),
            "autolink html: {html}"
        );
    }

    #[test]
    fn export_file_to_html_writes_temp_file() {
        let dir = crate::test_support::TempDir::new("export");
        let input = dir.join("note.md");
        std::fs::write(&input, "# Hello\n\n- [x] done\n").unwrap();

        let out = export_file_to_html(&input, None).unwrap();
        assert_eq!(out.extension().and_then(|e| e.to_str()), Some("html"));
        let html = std::fs::read_to_string(&out).unwrap();
        assert!(html.contains("Hello"));
        assert!(html.contains("checkbox") || html.contains("checked"));

        let custom = dir.join("nested/out.html");
        let written = export_file_to_html(&input, Some(&custom)).unwrap();
        assert_eq!(written, custom);
        assert!(custom.exists());
    }

    #[test]
    fn export_file_missing_input_errors() {
        let err =
            export_file_to_html(Path::new("/no/such/markrust-core-file.md"), None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn export_content_requires_a_path() {
        let err = export_content_to_html("# Hi", None, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn exports_links_and_images() {
        let html = markdown_to_html_gfm("[MarkRust](https://markrust.org) and ![alt](pic.png)");
        assert!(
            html.contains("href=\"https://markrust.org\""),
            "link html: {html}"
        );
        assert!(html.contains("MarkRust"), "link text html: {html}");
        assert!(
            html.contains("src=\"pic.png\"") || html.contains("pic.png"),
            "image html: {html}"
        );
    }

    #[test]
    fn write_markdown_creates_parent_dirs() {
        let dir = crate::test_support::TempDir::new("export-write");
        let output = dir.join("deep/nested/out.html");
        write_markdown_to_html_file("| A | B |\n|---|---|\n| 1 | 2 |\n", &output).unwrap();
        let html = std::fs::read_to_string(&output).unwrap();
        assert!(html.contains("<table>"));
    }
}
