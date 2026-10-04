// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Small embedded UI catalogs. Document text, Markdown syntax and file paths
//! are never translated. English keys are also the safe fallback for diagnostics.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, OnceLock},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "ru")]
    Russian,
    #[serde(rename = "es")]
    Spanish,
    #[serde(rename = "fr")]
    French,
    #[serde(rename = "de")]
    German,
    #[serde(rename = "pt")]
    Portuguese,
    #[serde(rename = "it")]
    Italian,
    #[serde(rename = "nl")]
    Dutch,
    #[serde(rename = "pl")]
    Polish,
    #[serde(rename = "uk")]
    Ukrainian,
    #[serde(rename = "tr")]
    Turkish,
    #[serde(rename = "ar")]
    Arabic,
    #[serde(rename = "he")]
    Hebrew,
    #[serde(rename = "hi")]
    Hindi,
    #[serde(rename = "bn")]
    Bengali,
    #[serde(rename = "zh")]
    Chinese,
    #[serde(rename = "ja")]
    Japanese,
    #[serde(rename = "ko")]
    Korean,
    #[serde(rename = "id")]
    Indonesian,
    #[serde(rename = "vi")]
    Vietnamese,
    #[default]
    #[serde(rename = "en", other)]
    English,
}

impl Language {
    pub const ALL: [Self; 20] = [
        Self::English,
        Self::Russian,
        Self::Spanish,
        Self::French,
        Self::German,
        Self::Portuguese,
        Self::Italian,
        Self::Dutch,
        Self::Polish,
        Self::Ukrainian,
        Self::Turkish,
        Self::Arabic,
        Self::Hebrew,
        Self::Hindi,
        Self::Bengali,
        Self::Chinese,
        Self::Japanese,
        Self::Korean,
        Self::Indonesian,
        Self::Vietnamese,
    ];

    pub fn native_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Russian => "Русский",
            Self::Spanish => "Español",
            Self::French => "Français",
            Self::German => "Deutsch",
            Self::Portuguese => "Português",
            Self::Italian => "Italiano",
            Self::Dutch => "Nederlands",
            Self::Polish => "Polski",
            Self::Ukrainian => "Українська",
            Self::Turkish => "Türkçe",
            Self::Arabic => "العربية",
            Self::Hebrew => "עברית",
            Self::Hindi => "हिन्दी",
            Self::Bengali => "বাংলা",
            Self::Chinese => "简体中文",
            Self::Japanese => "日本語",
            Self::Korean => "한국어",
            Self::Indonesian => "Bahasa Indonesia",
            Self::Vietnamese => "Tiếng Việt",
        }
    }

    pub fn is_rtl(self) -> bool {
        matches!(self, Self::Arabic | Self::Hebrew)
    }

    fn source(self) -> &'static str {
        match self {
            Self::English => include_str!("locales/en.json"),
            Self::Russian => include_str!("locales/ru.json"),
            Self::Spanish => include_str!("locales/es.json"),
            Self::French => include_str!("locales/fr.json"),
            Self::German => include_str!("locales/de.json"),
            Self::Portuguese => include_str!("locales/pt.json"),
            Self::Italian => include_str!("locales/it.json"),
            Self::Dutch => include_str!("locales/nl.json"),
            Self::Polish => include_str!("locales/pl.json"),
            Self::Ukrainian => include_str!("locales/uk.json"),
            Self::Turkish => include_str!("locales/tr.json"),
            Self::Arabic => include_str!("locales/ar.json"),
            Self::Hebrew => include_str!("locales/he.json"),
            Self::Hindi => include_str!("locales/hi.json"),
            Self::Bengali => include_str!("locales/bn.json"),
            Self::Chinese => include_str!("locales/zh.json"),
            Self::Japanese => include_str!("locales/ja.json"),
            Self::Korean => include_str!("locales/ko.json"),
            Self::Indonesian => include_str!("locales/id.json"),
            Self::Vietnamese => include_str!("locales/vi.json"),
        }
    }
}

type Catalog = Arc<BTreeMap<String, String>>;

pub fn catalog(language: Language) -> Catalog {
    static CATALOGS: OnceLock<Vec<Catalog>> = OnceLock::new();
    let catalogs = CATALOGS.get_or_init(|| {
        Language::ALL
            .iter()
            .map(|language| {
                Arc::new(
                    serde_json::from_str(language.source()).expect("validated embedded UI catalog"),
                )
            })
            .collect()
    });
    catalogs[Language::ALL
        .iter()
        .position(|candidate| *candidate == language)
        .unwrap()]
    .clone()
}

pub fn text(language: Language, key: &str) -> String {
    catalog(language)
        .get(key)
        .map_or(key, String::as_str)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct UniqueCatalog;

    impl<'de> serde::de::Visitor<'de> for UniqueCatalog {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a catalog with unique string keys")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut catalog = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if catalog.insert(key.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!("duplicate UI key: {key}")));
                }
            }
            Ok(catalog)
        }
    }

    fn placeholders(value: &str) -> Vec<&str> {
        let mut tokens = Vec::new();
        let mut tail = value;
        while let Some(start) = tail.find('{') {
            tail = &tail[start..];
            let end = tail.find('}').expect("unclosed translation placeholder");
            tokens.push(&tail[..=end]);
            tail = &tail[end + 1..];
        }
        tokens.sort_unstable();
        tokens
    }

    #[test]
    fn every_catalog_has_complete_nonempty_unique_ui_keys() {
        let english = catalog(Language::English);
        assert_eq!(Language::ALL.len(), 20);
        for language in Language::ALL {
            let mut deserializer = serde_json::Deserializer::from_str(language.source());
            let raw =
                serde::Deserializer::deserialize_map(&mut deserializer, UniqueCatalog).unwrap();
            deserializer.end().unwrap();
            let translated = catalog(language);
            assert_eq!(
                translated.keys().collect::<Vec<_>>(),
                english.keys().collect::<Vec<_>>(),
                "{}",
                language.native_name()
            );
            assert_eq!(&raw, translated.as_ref());
            assert!(
                translated.values().all(|value| !value.trim().is_empty()),
                "{}",
                language.native_name()
            );
            for unchanged in ["WYSIWYG", "MarkRust Help", "About MarkRust"] {
                assert!(translated.contains_key(unchanged));
            }
            for (key, value) in translated.iter() {
                assert_eq!(
                    placeholders(key),
                    placeholders(value),
                    "{}: {key}",
                    language.native_name()
                );
                assert!(
                    !value
                        .chars()
                        .any(|ch| matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
                    "logical Unicode order: {}: {key}",
                    language.native_name()
                );
            }
        }
    }

    #[test]
    fn duplicate_catalog_keys_are_rejected() {
        let mut deserializer =
            serde_json::Deserializer::from_str(r#"{"Save":"Save","Save":"Changed"}"#);
        assert!(serde::Deserializer::deserialize_map(&mut deserializer, UniqueCatalog).is_err());
    }

    #[test]
    fn unknown_language_and_unknown_labels_fail_safely_to_english() {
        assert_eq!(
            serde_json::from_str::<Language>("\"future-locale\"").unwrap(),
            Language::English
        );
        assert_eq!(
            text(Language::Russian, "Untranslated diagnostic"),
            "Untranslated diagnostic"
        );
        assert_ne!(text(Language::Russian, "Save"), "Save");
        assert_ne!(text(Language::Japanese, "Save"), "Save");
    }
}
