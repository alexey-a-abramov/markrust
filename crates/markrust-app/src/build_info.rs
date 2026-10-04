// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One compiled build identity for About, CLI diagnostics and bundle metadata.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const BUILT_AT_UTC: &str = env!("MARKRUST_BUILD_UTC");
pub const TIMESTAMP_SOURCE: &str = env!("MARKRUST_BUILD_TIMESTAMP_SOURCE");

pub fn about_details() -> String {
    let readable_utc = BUILT_AT_UTC.replace('T', " ").replace('Z', " UTC");
    let reproducible = if TIMESTAMP_SOURCE == "source-date-epoch" {
        " (reproducible build timestamp)"
    } else {
        ""
    };
    format!(
        "Version {VERSION}\nBuilt: {readable_utc}{reproducible}\nA native Markdown writing app.\nMozilla Public License 2.0"
    )
}

pub fn metadata_json() -> String {
    serde_json::json!({
        "version": VERSION,
        "built_at_utc": BUILT_AT_UTC,
        "timestamp_source": TIMESTAMP_SOURCE,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_and_json_share_the_compiled_identity() {
        let metadata: serde_json::Value = serde_json::from_str(&metadata_json()).unwrap();
        assert_eq!(metadata["version"], VERSION);
        assert_eq!(metadata["built_at_utc"], BUILT_AT_UTC);
        assert_eq!(metadata["timestamp_source"], TIMESTAMP_SOURCE);
        let parsed = time::OffsetDateTime::parse(
            BUILT_AT_UTC,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(parsed.offset(), time::UtcOffset::UTC);
        assert_eq!(BUILT_AT_UTC.len(), 20);
        assert!(about_details().contains(&format!("Version {VERSION}")));
        assert!(about_details().contains(&BUILT_AT_UTC.replace('T', " ").replace('Z', " UTC")));
        assert!(matches!(
            TIMESTAMP_SOURCE,
            "source-date-epoch" | "build-clock"
        ));
    }
}
