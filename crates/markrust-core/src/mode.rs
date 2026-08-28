// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
}
