// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const DEFAULT_AUTOSAVE_MS: u64 = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub theme: ThemeChoice,
    pub font_family: String,
    pub font_size: f32,
    pub code_font_family: String,
    pub autosave_ms: u64,
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
            theme: ThemeChoice::Dark,
            font_family: markrust_editor::default_ui_font().into(),
            font_size: 16.0,
            code_font_family: markrust_editor::default_ui_font().into(),
            autosave_ms: DEFAULT_AUTOSAVE_MS,
        }
    }
}

impl AppConfig {
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
        theme
    }

    /// Maps virtual / broken system names onto the bundled Inter family.
    ///
    /// GPUI's `.SystemUIFont` → `.AppleSystemUIFont` lookup often fails on
    /// recent macOS (glyphs layout but never rasterize), so we never use it.
    pub fn resolve_ui_font(family: &str) -> String {
        match family {
            "" | "Inter" | "system-ui" | ".SystemUIFont" | ".AppleSystemUIFont" | "SF Pro"
            | "SF Pro Text" | "SF Pro Display" => markrust_editor::default_ui_font().into(),
            other => other.to_string(),
        }
    }

    fn resolve_code_font(family: &str) -> String {
        match family {
            "" => markrust_editor::default_ui_font().into(),
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
        assert_eq!(
            AppConfig::default().font_family,
            markrust_editor::default_ui_font()
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
                markrust_editor::default_ui_font(),
                "alias {alias:?}"
            );
        }
        assert_eq!(AppConfig::resolve_ui_font("Menlo"), "Menlo");
        assert_eq!(AppConfig::resolve_ui_font("Helvetica"), "Helvetica");
    }
}
