// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Offline production-extractor probe; never installs or launches an artifact.

#[cfg(feature = "gui-tests")]
fn main() -> anyhow::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let archive = arguments
        .next()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: update_archive_probe ARCHIVE EXPECTED_VERSION"))?;
    let version = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| anyhow::anyhow!("expected version must be UTF-8"))?;
    anyhow::ensure!(arguments.next().is_none(), "unexpected probe arguments");
    let staged = markrust_app::updater::validate_local_archive_for_test(&archive, &version)?;
    println!(
        "{}",
        serde_json::json!({
            "status": "verified",
            "version": staged.version(),
            "archive_sha256": staged.archive_sha256(),
            "checks": ["bounded private copy", "bounded raw tar/PAX preflight", "safe extraction", "plist identity/version", "native Mach-O architecture", "codesign deep strict"],
            "network": false,
            "installation_changed": false,
            "downloaded_executable_run": false,
            "temporary_stage_removed_on_exit": true,
        })
    );
    Ok(())
}

#[cfg(not(feature = "gui-tests"))]
fn main() {
    eprintln!("update_archive_probe requires the gui-tests feature");
    std::process::exit(2);
}
