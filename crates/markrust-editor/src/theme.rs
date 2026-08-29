// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use gpui::Hsla;

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
        Self {
            background: gpui::hsla(0., 0., 0.118, 1.), // #1e1e1e
            editor_bg: gpui::hsla(0., 0., 0.118, 1.),
            chrome_bg: gpui::hsla(0., 0., 0.145, 1.), // #252526
            text: gpui::hsla(0., 0., 0.92, 1.),
            delimiter: gpui::hsla(210. / 360., 0.35, 0.55, 0.85),
            selection: gpui::hsla(210. / 360., 0.72, 0.52, 0.35),
            caret: gpui::hsla(0., 0., 0.95, 1.),
            sidebar_bg: gpui::hsla(0., 0., 0.176, 1.), // #2d2d2d
            sidebar_text: gpui::hsla(0., 0., 0.78, 1.),
            sidebar_hover: gpui::hsla(0., 0., 1.0, 0.06),
            sidebar_selected: gpui::hsla(210. / 360., 0.72, 0.52, 0.22),
            sidebar_selected_text: gpui::hsla(0., 0., 0.95, 1.),
            tab_active: gpui::hsla(0., 0., 0.118, 1.),
            tab_inactive: gpui::hsla(0., 0., 0.155, 1.),
            status_bar_bg: gpui::hsla(0., 0., 0.145, 1.),
            status_bar_text: gpui::hsla(0., 0., 0.58, 1.),
            secondary_text: gpui::hsla(0., 0., 0.52, 1.),
            accent: gpui::hsla(210. / 360., 0.95, 0.58, 1.), // macOS system blue
            toolbar_button_hover: gpui::hsla(0., 0., 1.0, 0.08),
            drop_zone_bg: gpui::hsla(210. / 360., 0.72, 0.52, 0.12),
            separator: gpui::hsla(0., 0., 1.0, 0.08),
            link: gpui::hsla(210. / 360., 0.75, 0.65, 1.),
            blockquote_text: gpui::hsla(0., 0., 0.72, 1.),
            blockquote_border: gpui::hsla(210. / 360., 0.85, 0.58, 1.),
            code_bg: gpui::hsla(0., 0., 0.22, 1.),
            code_block_bg: gpui::hsla(220. / 360., 0.12, 0.16, 1.),
            image_text: gpui::hsla(120. / 360., 0.25, 0.55, 1.),
            table_header_bg: gpui::hsla(210. / 360., 0.2, 0.22, 1.),
            table_delimiter: gpui::hsla(0., 0., 0.45, 1.),
            frontmatter_text: gpui::hsla(45. / 360., 0.35, 0.55, 1.),
            syntax_keyword: gpui::hsla(280. / 360., 0.55, 0.72, 1.),
            syntax_string: gpui::hsla(100. / 360., 0.45, 0.65, 1.),
            syntax_number: gpui::hsla(35. / 360., 0.65, 0.65, 1.),
            syntax_comment: gpui::hsla(0., 0., 0.5, 1.),
            syntax_function: gpui::hsla(210. / 360., 0.55, 0.72, 1.),
            syntax_type: gpui::hsla(35. / 360., 0.45, 0.72, 1.),
            font_family: ".SystemUIFont".into(),
            font_size: 16.0,
            code_font_family: "Menlo".into(),
            line_height_multiplier: 1.55,
        }
    }

    pub fn light() -> Self {
        Self {
            background: gpui::hsla(0., 0., 0.98, 1.),
            editor_bg: gpui::hsla(0., 0., 1.0, 1.),
            chrome_bg: gpui::hsla(0., 0., 0.96, 1.), // #f5f5f5
            text: gpui::hsla(0., 0., 0.12, 1.),
            delimiter: gpui::hsla(210. / 360., 0.35, 0.45, 0.85),
            selection: gpui::hsla(210. / 360., 0.72, 0.52, 0.22),
            caret: gpui::hsla(0., 0., 0.08, 1.),
            sidebar_bg: gpui::hsla(0., 0., 0.925, 1.), // #ececec
            sidebar_text: gpui::hsla(0., 0., 0.25, 1.),
            sidebar_hover: gpui::hsla(0., 0., 0.0, 0.05),
            sidebar_selected: gpui::hsla(210. / 360., 0.72, 0.52, 0.18),
            sidebar_selected_text: gpui::hsla(0., 0., 0.08, 1.),
            tab_active: gpui::hsla(0., 0., 1.0, 1.),
            tab_inactive: gpui::hsla(0., 0., 0.94, 1.),
            status_bar_bg: gpui::hsla(0., 0., 0.96, 1.),
            status_bar_text: gpui::hsla(0., 0., 0.45, 1.),
            secondary_text: gpui::hsla(0., 0., 0.52, 1.),
            accent: gpui::hsla(210. / 360., 0.95, 0.48, 1.),
            toolbar_button_hover: gpui::hsla(0., 0., 0.0, 0.06),
            drop_zone_bg: gpui::hsla(210. / 360., 0.72, 0.52, 0.10),
            separator: gpui::hsla(0., 0., 0.0, 0.08),
            link: gpui::hsla(210. / 360., 0.85, 0.42, 1.),
            blockquote_text: gpui::hsla(0., 0., 0.38, 1.),
            blockquote_border: gpui::hsla(210. / 360., 0.85, 0.48, 1.),
            code_bg: gpui::hsla(0., 0., 0.93, 1.),
            code_block_bg: gpui::hsla(220. / 360., 0.12, 0.95, 1.),
            image_text: gpui::hsla(120. / 360., 0.35, 0.38, 1.),
            table_header_bg: gpui::hsla(210. / 360., 0.15, 0.92, 1.),
            table_delimiter: gpui::hsla(0., 0., 0.55, 1.),
            frontmatter_text: gpui::hsla(45. / 360., 0.45, 0.38, 1.),
            syntax_keyword: gpui::hsla(280. / 360., 0.65, 0.45, 1.),
            syntax_string: gpui::hsla(100. / 360., 0.55, 0.35, 1.),
            syntax_number: gpui::hsla(35. / 360., 0.75, 0.4, 1.),
            syntax_comment: gpui::hsla(0., 0., 0.55, 1.),
            syntax_function: gpui::hsla(210. / 360., 0.65, 0.42, 1.),
            syntax_type: gpui::hsla(35. / 360., 0.55, 0.42, 1.),
            font_family: ".SystemUIFont".into(),
            font_size: 16.0,
            code_font_family: "Menlo".into(),
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

    pub fn system_font_fallbacks() -> gpui::FontFallbacks {
        gpui::FontFallbacks::from_fonts(vec![".SystemUIFont".into(), "Menlo".into()])
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
        assert!((theme.status_bar_bg.h - theme.accent.h).abs() > 0.01);
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
        assert!(!dark.syntax_keyword.eq(&dark.text));
        assert!(!light.link.eq(&light.text));
    }
}
