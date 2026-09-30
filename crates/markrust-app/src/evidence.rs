// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Offline review of fixture-only action, state, and native-paint evidence.

use std::path::Path;

use anyhow::{ensure, Context, Result};
use serde_json::json;

use crate::usecases::Snapshot;

const TEMPLATE: &str = include_str!("../tests/ui-review.html");
const DATA_MARKER: &str = "MARKRUST_EVIDENCE_JSON";

/// Write beside the scenario's JSONL/report/frame files. This is intentionally
/// a fixture-test artifact, not a recorder for the user's documents.
pub(crate) fn write_review(
    path: &Path,
    scenario: &str,
    snapshots: &[Snapshot],
    error: Option<&str>,
    frames: bool,
) -> Result<()> {
    validate_name(scenario)?;
    let payload = json!({
        "schema_version": 1,
        "scenario": scenario,
        "error": error,
        "frames": frames,
        "snapshots": snapshots,
    });
    let html = render_review(&payload)?;
    let output = path.join(format!("{scenario}.review.html"));
    std::fs::write(&output, html)
        .with_context(|| format!("writing offline UI review {}", output.display()))
}

pub(crate) fn validate_name(scenario: &str) -> Result<()> {
    ensure!(
        !scenario.is_empty()
            && scenario
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'),
        "scenario evidence name must be a portable filename component"
    );
    Ok(())
}

fn render_review(payload: &serde_json::Value) -> Result<String> {
    let serialized = serde_json::to_string(payload)?;
    // JSON parsing alone does not stop the HTML parser from seeing </script>.
    // Escape every HTML-sensitive character before embedding fixture content.
    let embedded = serialized
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    Ok(TEMPLATE.replacen(DATA_MARKER, &embedded, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_html_cannot_escape_the_inert_json_script() {
        let source = "</script><script>alert('fixture')</script><img src=x onerror=alert(1)>&\u{2028}\u{2029}";
        let payload = json!({ "source": source });
        let html = render_review(&payload).unwrap();
        let prefix = "<script type=\"application/json\" id=\"evidence-data\">";
        let embedded = html
            .split_once(prefix)
            .unwrap()
            .1
            .split_once("</script>")
            .unwrap()
            .0;
        assert!(!embedded.contains(['<', '>', '&', '\u{2028}', '\u{2029}']));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(embedded).unwrap(),
            payload
        );
        assert_eq!(html.matches("<script").count(), 2);
        assert!(!html.contains("<img src=x"));
    }

    #[test]
    fn review_template_uses_local_resources_and_text_nodes() {
        assert_eq!(TEMPLATE.matches(DATA_MARKER).count(), 1);
        assert!(!TEMPLATE.contains("https://"));
        assert!(!TEMPLATE.contains("http://"));
        assert!(!TEMPLATE.contains("innerHTML"));
        assert!(!TEMPLATE.contains("eval("));
        assert!(!TEMPLATE.contains("fetch("));
    }

    #[test]
    fn rejects_paths_before_writing_evidence() {
        for name in ["../outside", "/tmp/outside", "", "bad/name", "bad.name"] {
            assert!(validate_name(name).is_err());
        }
        assert!(validate_name("split_source-123").is_ok());
    }
}
