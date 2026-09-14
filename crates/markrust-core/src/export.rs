// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use comrak::{markdown_to_html, Options};

/// Controls whether an HTML export can include content that is unsafe for
/// untrusted Markdown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HtmlExportPolicy {
    /// Omit raw HTML and neutralize URL schemes that Comrak considers dangerous.
    #[default]
    Safe,
    /// Preserve the historical Comrak unsafe-rendering behavior for Markdown
    /// that the caller trusts. GFM tag filtering remains enabled, but permitted
    /// raw HTML and dangerous Markdown URL schemes are preserved.
    Trusted,
}

/// Render Markdown source to an HTML fragment using GFM extensions plus
/// Typora extras that the rich tree also parses (footnotes, description lists,
/// dollar math, GitHub alerts, wikilinks).
pub fn markdown_to_html_gfm(source: &str) -> String {
    markdown_to_html_gfm_with_policy(source, HtmlExportPolicy::default())
}

/// Render Markdown source to an HTML fragment using GFM extensions and `policy`.
pub fn markdown_to_html_gfm_with_policy(source: &str, policy: HtmlExportPolicy) -> String {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.description_lists = true;
    options.extension.superscript = true;
    options.extension.subscript = true;
    options.extension.math_dollars = true;
    options.extension.alerts = true;
    options.extension.wikilinks_title_after_pipe = true;
    options.extension.tagfilter = true;
    options.extension.front_matter_delimiter = Some("---".into());
    options.render.unsafe_ = matches!(policy, HtmlExportPolicy::Trusted);
    let parse_input = crate::frontmatter::comrak_parse_input(source);
    markdown_to_html(parse_input.as_ref(), &options)
}

/// Write Markdown source to an HTML file at `output`.
pub fn write_markdown_to_html_file(source: &str, output: &Path) -> io::Result<()> {
    write_markdown_to_html_file_with_policy(source, output, HtmlExportPolicy::default())
}

/// Write Markdown source to an HTML file at `output` using `policy`.
pub fn write_markdown_to_html_file_with_policy(
    source: &str,
    output: &Path,
    policy: HtmlExportPolicy,
) -> io::Result<()> {
    let html = markdown_to_html_gfm_with_policy(source, policy);
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
    export_content_to_html_with_policy(source, input_path, output, HtmlExportPolicy::default())
}

/// Export Markdown source to HTML using `policy`, inferring the output path from
/// `input_path` when needed.
pub fn export_content_to_html_with_policy(
    source: &str,
    input_path: Option<&Path>,
    output: Option<&Path>,
    policy: HtmlExportPolicy,
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
    write_markdown_to_html_file_with_policy(source, &out_path, policy)?;
    Ok(out_path)
}

/// Read a Markdown file and write HTML to `output` (or `<stem>.html` beside the source).
pub fn export_file_to_html(input: &Path, output: Option<&Path>) -> io::Result<PathBuf> {
    export_file_to_html_with_policy(input, output, HtmlExportPolicy::default())
}

/// Read a Markdown file and write HTML to `output` (or `<stem>.html` beside the
/// source) using `policy`.
pub fn export_file_to_html_with_policy(
    input: &Path,
    output: Option<&Path>,
    policy: HtmlExportPolicy,
) -> io::Result<PathBuf> {
    let source = fs::read_to_string(input)?;
    export_content_to_html_with_policy(&source, Some(input), output, policy)
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
    fn safe_export_omits_raw_html_and_dangerous_urls() {
        let md = concat!(
            "<script>alert('nope')</script>\n\n",
            "<span class=\"badge\">raw HTML</span>\n\n",
            "[bad link](javascript:alert(1))\n\n",
            "![bad image](javascript:alert(2))\n",
        );
        let html = markdown_to_html_gfm(md);

        assert_eq!(HtmlExportPolicy::default(), HtmlExportPolicy::Safe);
        assert!(!html.contains("<script"), "script must not render: {html}");
        assert!(
            !html.contains("<span class=\"badge\">"),
            "raw HTML must not render: {html}"
        );
        assert!(
            !html.contains("javascript:"),
            "dangerous URLs must not render: {html}"
        );
    }

    #[test]
    fn trusted_export_preserves_benign_raw_html() {
        let html = markdown_to_html_gfm_with_policy(
            "<span class=\"badge\">trusted HTML</span>",
            HtmlExportPolicy::Trusted,
        );

        assert!(
            html.contains("<span class=\"badge\">trusted HTML</span>"),
            "trusted raw HTML must render: {html}"
        );
    }

    #[test]
    fn write_and_file_exports_default_to_safe_policy() {
        let dir = crate::test_support::TempDir::new("export-policy");
        let md = "<span class=\"badge\">raw HTML</span>";

        let written = dir.join("written.html");
        write_markdown_to_html_file(md, &written).unwrap();
        let written_html = fs::read_to_string(&written).unwrap();
        assert!(
            !written_html.contains("<span class=\"badge\">"),
            "default writer must be safe: {written_html}"
        );

        let input = dir.join("input.md");
        fs::write(&input, md).unwrap();
        let exported = dir.join("exported.html");
        export_file_to_html(&input, Some(&exported)).unwrap();
        let exported_html = fs::read_to_string(&exported).unwrap();
        assert!(
            !exported_html.contains("<span class=\"badge\">"),
            "default file export must be safe: {exported_html}"
        );
    }

    #[test]
    fn exports_frontmatter_without_rendering_fence() {
        for md in [
            "---\ntitle: Test\n---\n\n# Body\n",
            "---\ntitle: Test\n...\n\n# Body\n",
        ] {
            let html = markdown_to_html_gfm(md);
            assert!(
                html.contains("<h1>Body</h1>") || html.contains("Body</h1>"),
                "{md:?} html={html}"
            );
            assert!(!html.contains("title: Test"), "{md:?} html={html}");
        }
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
    fn exports_dollar_math() {
        let html = markdown_to_html_gfm("see $x^2$ and $$E=mc^2$$ and $5");
        assert!(
            html.contains("data-math-style=\"inline\"") && html.contains("x^2"),
            "inline math html: {html}"
        );
        assert!(
            html.contains("data-math-style=\"display\"") && html.contains("E=mc^2"),
            "display math html: {html}"
        );
        assert!(
            html.contains("$5"),
            "currency must remain text, html: {html}"
        );
    }

    #[test]
    fn exports_wikilinks() {
        let html = markdown_to_html_gfm("see [[page]] and [[page|Label]]");
        assert!(
            html.contains("data-wikilink=\"true\"") && html.contains("href=\"page\""),
            "wikilink html: {html}"
        );
        assert!(html.contains("Label"), "piped label html: {html}");
        assert!(
            !html.contains("[[page]]"),
            "raw wiki chrome must not appear: {html}"
        );
    }

    #[test]
    fn exports_github_alerts() {
        for (tag, class) in [
            ("NOTE", "markdown-alert-note"),
            ("TIP", "markdown-alert-tip"),
            ("IMPORTANT", "markdown-alert-important"),
            ("WARNING", "markdown-alert-warning"),
            ("CAUTION", "markdown-alert-caution"),
        ] {
            let md = format!("> [!{tag}]\n> body\n");
            let html = markdown_to_html_gfm(&md);
            assert!(
                html.contains("markdown-alert") && html.contains(class),
                "{tag} html: {html}"
            );
            assert!(
                !html.contains(&format!("[!{tag}]")),
                "raw tag must not appear in html for {tag}: {html}"
            );
        }
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
