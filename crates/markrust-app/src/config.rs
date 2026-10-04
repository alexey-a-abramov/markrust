// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const DEFAULT_AUTOSAVE_MS: u64 = 150;

fn default_markup_hints_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub language: crate::i18n::Language,
    #[serde(default = "default_automatic_updates")]
    pub automatic_updates: bool,
    pub theme: ThemeChoice,
    pub font_family: String,
    pub font_size: f32,
    pub code_font_family: String,
    /// Private session checkpoint delay, never an implicit document-file save.
    /// The existing preference name is retained for older configuration files.
    #[serde(default = "default_autosave_ms")]
    pub autosave_ms: u64,
    #[serde(default = "default_markup_hints_enabled")]
    pub markup_hints_enabled: bool,
    #[serde(default)]
    pub highlight_style: HighlightStyle,
}

fn default_autosave_ms() -> u64 {
    DEFAULT_AUTOSAVE_MS
}

fn default_automatic_updates() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HighlightStyle {
    #[default]
    Native,
    Ocean,
    Forest,
}

impl HighlightStyle {
    fn palette(self) -> markrust_editor::theme::HighlightPalette {
        use markrust_editor::theme::HighlightPalette;
        match self {
            Self::Native => HighlightPalette::Native,
            Self::Ocean => HighlightPalette::Ocean,
            Self::Forest => HighlightPalette::Forest,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    Dark,
    Light,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: crate::i18n::Language::default(),
            automatic_updates: true,
            theme: ThemeChoice::Dark,
            font_family: "Inter".into(),
            font_size: 16.0,
            code_font_family: "Menlo".into(),
            autosave_ms: DEFAULT_AUTOSAVE_MS,
            markup_hints_enabled: true,
            highlight_style: HighlightStyle::default(),
        }
    }
}

impl AppConfig {
    /// Legacy configurations may contain a long source-autosave delay. Private
    /// draft recovery has a bounded latency and cannot be disabled by zero.
    pub fn private_checkpoint_delay_ms(&self) -> u64 {
        self.autosave_ms.clamp(25, 150)
    }
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("markrust")
    }

    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    pub fn recent_path() -> PathBuf {
        Self::config_dir().join("recent.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if !path.exists() {
            let config = Self::default();
            let _ = config.save();
            return config;
        }
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(Self::config_dir())?;
        let raw = toml::to_string_pretty(self).unwrap_or_default();
        std::fs::write(Self::config_path(), raw)
    }

    pub fn editor_theme(&self) -> markrust_editor::EditorTheme {
        let mut theme = match self.theme {
            ThemeChoice::Dark => markrust_editor::EditorTheme::dark(),
            ThemeChoice::Light => markrust_editor::EditorTheme::light(),
        };
        theme.font_family = Self::resolve_ui_font(&self.font_family);
        theme.font_size = self.font_size;
        theme.code_font_family = Self::resolve_code_font(&self.code_font_family);
        theme.apply_highlight_palette(self.highlight_style.palette());
        theme.ui_strings = crate::i18n::catalog(self.language);
        theme
    }

    /// Maps virtual / broken system names onto the bundled Inter family.
    ///
    /// GPUI's `.SystemUIFont` → `.AppleSystemUIFont` lookup often fails on
    /// recent macOS (glyphs layout but never rasterize), so we never use it.
    pub fn resolve_ui_font(family: &str) -> String {
        match family {
            "" | "Inter" | "system-ui" | ".SystemUIFont" | ".AppleSystemUIFont" | "SF Pro"
            | "SF Pro Text" | "SF Pro Display" => "Inter".into(),
            other => other.to_string(),
        }
    }

    fn resolve_code_font(family: &str) -> String {
        match family {
            "" => "Menlo".into(),
            other => other.to_string(),
        }
    }

    pub fn toggle_theme(&mut self) {
        self.theme = match self.theme {
            ThemeChoice::Dark => ThemeChoice::Light,
            ThemeChoice::Light => ThemeChoice::Dark,
        };
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecentWorkspaces {
    pub workspaces: Vec<PathBuf>,
    #[serde(default)]
    pub files: Vec<PathBuf>,
}

impl RecentWorkspaces {
    pub fn load() -> Self {
        let path = AppConfig::recent_path();
        if !path.exists() {
            return Self::default();
        }
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(AppConfig::config_dir())?;
        std::fs::write(
            AppConfig::recent_path(),
            serde_json::to_string_pretty(self).unwrap_or_default(),
        )
    }

    pub fn push(&mut self, path: PathBuf) {
        self.workspaces.retain(|existing| existing != &path);
        self.workspaces.insert(0, path);
        self.workspaces.truncate(10);
    }

    pub fn push_file(&mut self, path: PathBuf) {
        self.files.retain(|existing| existing != &path);
        self.files.insert(0, path);
        self.files.truncate(20);
    }
}

pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown" | "txt"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_autosave() {
        assert_eq!(AppConfig::default().autosave_ms, DEFAULT_AUTOSAVE_MS);
        assert_eq!(AppConfig::default().font_family, "Inter");
        assert!(AppConfig::default().markup_hints_enabled);
        assert!(AppConfig::default().automatic_updates);
        assert_eq!(AppConfig::default().highlight_style, HighlightStyle::Native);
    }

    #[test]
    fn legacy_autosave_preference_only_bounds_private_checkpoint_latency() {
        let mut config = AppConfig {
            autosave_ms: 5000,
            ..AppConfig::default()
        };
        assert_eq!(config.private_checkpoint_delay_ms(), 150);
        config.autosave_ms = 0;
        assert_eq!(config.private_checkpoint_delay_ms(), 25);
    }

    #[test]
    fn older_config_keeps_markup_hints_enabled() {
        let config: AppConfig = toml::from_str(
            "theme = 'dark'\nfont_family = 'Inter'\nfont_size = 16.0\ncode_font_family = 'Menlo'\nautosave_ms = 1000\n",
        )
        .unwrap();
        assert!(config.markup_hints_enabled);
        assert!(config.automatic_updates);
        assert_eq!(config.highlight_style, HighlightStyle::Native);
        assert_eq!(config.language, crate::i18n::Language::English);
    }

    #[test]
    fn automatic_update_opt_out_survives_configuration_roundtrip() {
        let config = AppConfig {
            automatic_updates: false,
            language: crate::i18n::Language::Russian,
            ..AppConfig::default()
        };
        let restored: AppConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert!(!restored.automatic_updates);
        assert_eq!(restored.language, config.language);
        assert_eq!(restored.theme, config.theme);
        assert_eq!(restored.font_family, config.font_family);
    }

    #[test]
    fn language_roundtrips_without_resetting_other_preferences() {
        for language in crate::i18n::Language::ALL {
            let config = AppConfig {
                language,
                theme: ThemeChoice::Light,
                font_size: 19.,
                highlight_style: HighlightStyle::Ocean,
                markup_hints_enabled: false,
                ..AppConfig::default()
            };
            let raw = toml::to_string(&config).unwrap();
            let restored: AppConfig = toml::from_str(&raw).unwrap();
            assert_eq!(restored.language, language);
            assert_eq!(restored.theme, ThemeChoice::Light);
            assert_eq!(restored.font_size, 19.);
            assert_eq!(restored.highlight_style, HighlightStyle::Ocean);
            assert!(!restored.markup_hints_enabled);
            let future: AppConfig = toml::from_str(&raw.replace(
                &format!(
                    "language = \"{}\"",
                    serde_json::to_value(language).unwrap().as_str().unwrap()
                ),
                "language = \"future-language\"",
            ))
            .unwrap();
            assert_eq!(future.language, crate::i18n::Language::English);
            assert_eq!(future.font_size, 19.);
            assert_eq!(future.theme, ThemeChoice::Light);
        }
    }

    #[test]
    fn highlight_choice_roundtrips_and_keeps_typography() {
        for style in [
            HighlightStyle::Native,
            HighlightStyle::Ocean,
            HighlightStyle::Forest,
        ] {
            let config = AppConfig {
                highlight_style: style,
                ..AppConfig::default()
            };
            let roundtrip: AppConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(roundtrip.highlight_style, style);
            assert_eq!(roundtrip.editor_theme().font_size, config.font_size);
        }
    }

    #[test]
    fn old_recent_workspace_file_migrates_without_losing_paths() {
        let mut recent: RecentWorkspaces =
            serde_json::from_str(r#"{"workspaces":["/workspace"]}"#).unwrap();
        assert_eq!(recent.workspaces, [PathBuf::from("/workspace")]);
        assert!(recent.files.is_empty());
        recent.push_file(PathBuf::from("/workspace/first.md"));
        recent.push_file(PathBuf::from("/workspace/second.md"));
        recent.push_file(PathBuf::from("/workspace/first.md"));
        assert_eq!(
            recent.files,
            [
                PathBuf::from("/workspace/first.md"),
                PathBuf::from("/workspace/second.md")
            ]
        );
    }

    #[test]
    fn system_ui_font_aliases_resolve_to_bundled_inter() {
        for alias in [
            "",
            "Inter",
            "system-ui",
            ".SystemUIFont",
            ".AppleSystemUIFont",
            "SF Pro Text",
        ] {
            assert_eq!(
                AppConfig::resolve_ui_font(alias),
                "Inter",
                "alias {alias:?}"
            );
        }
        assert_eq!(AppConfig::resolve_ui_font("Menlo"), "Menlo");
        assert_eq!(AppConfig::resolve_ui_font("Helvetica"), "Helvetica");
    }
}
