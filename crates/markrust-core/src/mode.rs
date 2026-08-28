// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::Path;

/// Controls whether Markdown parsing and WYSIWYG delimiter masking apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocumentProcessingMode {
    /// Parse Markdown and expose syntax spans for seamless WYSIWYG rendering.
    #[default]
    MarkdownWysiwyg,
    /// Skip tree-sitter; treat the buffer as plain text.
    PlainText,
}

impl DocumentProcessingMode {
    pub fn parses_markdown(self) -> bool {
        matches!(self, Self::MarkdownWysiwyg)
    }

    /// Choose a processing mode from the file extension.
    ///
    /// Markdown-like extensions (`md`, `markdown`, `mdown`, `mkd`, `mkdn`, `mdx`)
    /// map to [`Self::MarkdownWysiwyg`]. Everything else, including `.txt` and
    /// extension-less names, maps to [`Self::PlainText`]. Comparison is
    /// case-insensitive.
    pub fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref()
        {
            Some("md" | "markdown" | "mdown" | "mkd" | "mkdn" | "mdx") => Self::MarkdownWysiwyg,
            _ => Self::PlainText,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_markdown_flags() {
        assert!(DocumentProcessingMode::MarkdownWysiwyg.parses_markdown());
        assert!(!DocumentProcessingMode::PlainText.parses_markdown());
        assert!(DocumentProcessingMode::default().parses_markdown());
    }

    #[test]
    fn path_extension_selects_mode() {
        let cases: &[(&str, DocumentProcessingMode)] = &[
            ("notes.md", DocumentProcessingMode::MarkdownWysiwyg),
            ("NOTES.MD", DocumentProcessingMode::MarkdownWysiwyg),
            ("readme.markdown", DocumentProcessingMode::MarkdownWysiwyg),
            ("file.mdown", DocumentProcessingMode::MarkdownWysiwyg),
            ("file.mkd", DocumentProcessingMode::MarkdownWysiwyg),
            ("file.mkdn", DocumentProcessingMode::MarkdownWysiwyg),
            ("component.mdx", DocumentProcessingMode::MarkdownWysiwyg),
            ("notes.txt", DocumentProcessingMode::PlainText),
            ("main.rs", DocumentProcessingMode::PlainText),
            ("Makefile", DocumentProcessingMode::PlainText),
            ("no-extension", DocumentProcessingMode::PlainText),
        ];
        for &(path, expected) in cases {
            assert_eq!(
                DocumentProcessingMode::from_path(Path::new(path)),
                expected,
                "path {path}"
            );
        }
    }
}
