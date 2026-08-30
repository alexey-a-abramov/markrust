// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gpui::{rgb, Hsla};

fn hex(color: u32) -> Hsla {
    Hsla::from(rgb(color))
}

/// Default UI face on macOS: Menlo is a real Core Text family and rasterizes
/// reliably. `.SystemUIFont` / Inter-via-add_fonts currently paint empty glyphs.
pub fn default_ui_font() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    }
}

/// Editor color and typography settings.
#[derive(Debug, Clone)]
pub struct EditorTheme {
    pub background: Hsla,
    pub text: Hsla,
    pub delimiter: Hsla,
    pub selection: Hsla,
    pub caret: Hsla,
    pub sidebar_bg: Hsla,
    pub sidebar_text: Hsla,
    pub tab_active: Hsla,
    pub tab_inactive: Hsla,
    pub status_bar_bg: Hsla,
    pub status_bar_text: Hsla,
    pub accent: Hsla,
    pub link: Hsla,
    pub blockquote_text: Hsla,
    pub blockquote_border: Hsla,
    pub image_text: Hsla,
    pub table_header_bg: Hsla,
    pub table_delimiter: Hsla,
    pub frontmatter_text: Hsla,
    pub syntax_keyword: Hsla,
    pub syntax_string: Hsla,
    pub syntax_number: Hsla,
    pub syntax_comment: Hsla,
    pub syntax_function: Hsla,
    pub syntax_type: Hsla,
    pub font_family: String,
    pub font_size: f32,
    pub code_font_family: String,
    pub line_height_multiplier: f32,
    /// Window chrome (toolbar, tab bar background).
    pub chrome_bg: Hsla,
    /// Editor pane background (slightly distinct from chrome).
    pub editor_bg: Hsla,
    /// Separator lines between panels.
    pub separator: Hsla,
    /// Sidebar row hover background.
    pub sidebar_hover: Hsla,
    /// Sidebar selected row background.
    pub sidebar_selected: Hsla,
    /// Sidebar selected row text.
    pub sidebar_selected_text: Hsla,
    /// Muted secondary text (status bar, section hints).
    pub secondary_text: Hsla,
    /// Toolbar button hover background.
    pub toolbar_button_hover: Hsla,
    /// Drag-over highlight background.
    pub drop_zone_bg: Hsla,
    /// Inline code chip background.
    pub code_bg: Hsla,
    /// Fenced code block line background.
    pub code_block_bg: Hsla,
}

impl EditorTheme {
    pub fn dark() -> Self {
        // Warm iA Writer / Typora dark, rust accent (MarkRust).
        Self {
            background: hex(0x1c1917),
            editor_bg: hex(0x1c1917),
            chrome_bg: hex(0x292524),
            text: hex(0xf5f0e8),
            delimiter: hex(0xa8a29e),
            selection: hex(0x9a3412).opacity(0.35),
            caret: hex(0xfafaf9),
            sidebar_bg: hex(0x241f1c),
            sidebar_text: hex(0xe7e5e4),
            sidebar_hover: hex(0xfafaf9).opacity(0.06),
            sidebar_selected: hex(0xc2410c).opacity(0.28),
            sidebar_selected_text: hex(0xfff7ed),
            tab_active: hex(0x1c1917),
            tab_inactive: hex(0x241f1c),
            status_bar_bg: hex(0x1c1917),
            status_bar_text: hex(0xa8a29e),
            secondary_text: hex(0xa8a29e),
            accent: hex(0xc2410c),
            toolbar_button_hover: hex(0xfafaf9).opacity(0.08),
            drop_zone_bg: hex(0xc2410c).opacity(0.16),
            separator: hex(0xfafaf9).opacity(0.08),
            link: hex(0xfb923c),
            blockquote_text: hex(0xd6d3d1),
            blockquote_border: hex(0xea580c),
            code_bg: hex(0x292524),
            code_block_bg: hex(0x0c0a09),
            image_text: hex(0x86efac),
            table_header_bg: hex(0x292524),
            table_delimiter: hex(0x78716c),
            frontmatter_text: hex(0xfbbf24),
            syntax_keyword: hex(0xf0abfc),
            syntax_string: hex(0x86efac),
            syntax_number: hex(0xfdba74),
            syntax_comment: hex(0x78716c),
            syntax_function: hex(0x7dd3fc),
            syntax_type: hex(0xfcd34d),
            font_family: default_ui_font().into(),
            font_size: 16.0,
            code_font_family: default_ui_font().into(),
            line_height_multiplier: 1.55,
        }
    }

    pub fn light() -> Self {
        // iA Writer / Typora warm paper.
        Self {
            background: hex(0xfaf7f2),
            editor_bg: hex(0xfffcf7),
            chrome_bg: hex(0xf3eee7),
            text: hex(0x1c1917),
            delimiter: hex(0x78716c),
            selection: hex(0xea580c).opacity(0.18),
            caret: hex(0x1c1917),
            sidebar_bg: hex(0xeee8e0),
            sidebar_text: hex(0x44403c),
            sidebar_hover: hex(0x1c1917).opacity(0.05),
            sidebar_selected: hex(0xea580c).opacity(0.16),
            sidebar_selected_text: hex(0x1c1917),
            tab_active: hex(0xfffcf7),
            tab_inactive: hex(0xe7e0d6),
            status_bar_bg: hex(0xf3eee7),
            status_bar_text: hex(0x78716c),
            secondary_text: hex(0x78716c),
            accent: hex(0x9a3412),
            toolbar_button_hover: hex(0x1c1917).opacity(0.06),
            drop_zone_bg: hex(0xea580c).opacity(0.12),
            separator: hex(0x1c1917).opacity(0.08),
            link: hex(0xc2410c),
            blockquote_text: hex(0x57534e),
            blockquote_border: hex(0xea580c),
            code_bg: hex(0xf5efe6),
            code_block_bg: hex(0xf3eee7),
            image_text: hex(0x166534),
            table_header_bg: hex(0xf3eee7),
            table_delimiter: hex(0xa8a29e),
            frontmatter_text: hex(0xa16207),
            syntax_keyword: hex(0x7e22ce),
            syntax_string: hex(0x15803d),
            syntax_number: hex(0xc2410c),
            syntax_comment: hex(0x78716c),
            syntax_function: hex(0x1d4ed8),
            syntax_type: hex(0xb45309),
            font_family: default_ui_font().into(),
            font_size: 16.0,
            code_font_family: default_ui_font().into(),
            line_height_multiplier: 1.55,
        }
    }

    pub fn heading_font_size(&self, level: u8) -> f32 {
        let scale = match level {
            1 => 2.0,
            2 => 1.6,
            3 => 1.35,
            4 => 1.2,
            5 => 1.1,
            _ => 1.05,
        };
        self.font_size * scale
    }

    pub fn stable_line_height(&self, font_size: f32) -> f32 {
        // Use max metrics (heading scale 2.0) so mask toggles never reflow lines.
        font_size.max(self.font_size * 2.0) * self.line_height_multiplier
    }

    /// Line box for a painted run. Headings use their own size; body stays compact.
    pub fn line_height_for_font_size(&self, font_size: f32) -> f32 {
        font_size * self.line_height_multiplier
    }

    pub fn system_font_fallbacks() -> Option<gpui::FontFallbacks> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_line_height_uses_heading_max() {
        let theme = EditorTheme::dark();
        assert!(theme.stable_line_height(theme.font_size) >= theme.font_size * 2.0);
    }

    #[test]
    fn dark_status_bar_is_muted_not_accent() {
        let theme = EditorTheme::dark();
        assert_ne!(theme.status_bar_bg, theme.accent);
    }

    #[test]
    fn heading_one_is_larger_than_body() {
        let theme = EditorTheme::dark();
        assert!(theme.heading_font_size(1) > theme.font_size);
        assert!(
            theme.line_height_for_font_size(theme.font_size)
                < theme.stable_line_height(theme.font_size)
        );
    }

    #[test]
    fn code_block_contrasts_with_editor_background() {
        let dark = EditorTheme::dark();
        let light = EditorTheme::light();
        assert!((dark.code_block_bg.l - dark.editor_bg.l).abs() > 0.02);
        assert!((light.code_block_bg.l - light.editor_bg.l).abs() > 0.02);
    }

    #[test]
    fn heading_scale_decreases_with_level() {
        let theme = EditorTheme::dark();
        assert!(theme.heading_font_size(1) > theme.heading_font_size(2));
        assert!(theme.heading_font_size(2) > theme.heading_font_size(3));
        assert!(theme.heading_font_size(5) >= theme.heading_font_size(6));
        assert_eq!(theme.heading_font_size(9), theme.font_size * 1.05);
    }

    #[test]
    fn light_text_is_darker_than_dark_theme_text() {
        let dark = EditorTheme::dark();
        let light = EditorTheme::light();
        assert!(light.text.l < dark.text.l);
        assert!(light.background.l > dark.background.l);
        assert_eq!(dark.font_size, light.font_size);
        assert_eq!(dark.line_height_multiplier, light.line_height_multiplier);
        assert!(!dark.font_family.is_empty());
        assert!(dark.syntax_keyword.a > 0.9);
        assert!(light.link.a > 0.9);
    }
}
