// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI e2e: spawn the built `markrust` binary. Never pass empty args / `--gui`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::Command;
use markrust::{parse_args, run_export, CliAction};

fn fixture_showcase() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../markrust-app/tests/fixtures/showcase.md")
}

#[test]
fn cli_version_via_assert_cmd() {
    let mut cmd = Command::cargo_bin("markrust").unwrap();
    let output = cmd.arg("--version").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("markrust"), "{stdout}");
    assert!(!stdout.to_lowercase().contains("unknown"));
}

#[test]
fn cli_export_via_assert_cmd_and_library() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("markrust-cli-export-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("showcase.html");
    let fixture = fixture_showcase();

    let code = run_export(&[
        fixture.display().to_string(),
        "-o".into(),
        out.display().to_string(),
    ]);
    assert_eq!(code, 0);
    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("<table>"));
    assert!(
        html.contains("checkbox")
            || html.contains("task-list")
            || html.contains("type=\"checkbox\"")
    );

    let out2 = dir.join("cli.html");
    let mut cmd = Command::cargo_bin("markrust").unwrap();
    cmd.args([
        "export",
        fixture.to_str().unwrap(),
        "-o",
        out2.to_str().unwrap(),
    ])
    .assert()
    .success();
    let html2 = std::fs::read_to_string(&out2).unwrap();
    assert!(html2.contains("<table>"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_export_is_safe_by_default_and_can_trust_raw_html() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("markrust-cli-export-policy-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("input.md");
    std::fs::write(
        &input,
        concat!(
            "<script>alert('nope')</script>\n\n",
            "<span class=\"badge\">trusted HTML</span>\n\n",
            "[bad link](javascript:alert(1))\n\n",
            "![bad image](javascript:alert(2))\n",
        ),
    )
    .unwrap();

    let safe_output = dir.join("safe.html");
    let mut safe = Command::cargo_bin("markrust").unwrap();
    safe.args([
        "export",
        input.to_str().unwrap(),
        "--output",
        safe_output.to_str().unwrap(),
    ])
    .assert()
    .success();
    let safe_html = std::fs::read_to_string(&safe_output).unwrap();
    assert!(!safe_html.contains("<script"), "{safe_html}");
    assert!(!safe_html.contains("<span class=\"badge\">"), "{safe_html}");
    assert!(!safe_html.contains("javascript:"), "{safe_html}");

    let trusted_output = dir.join("trusted.html");
    let mut trusted = Command::cargo_bin("markrust").unwrap();
    trusted
        .args([
            "export",
            "--unsafe-html",
            input.to_str().unwrap(),
            "--output",
            trusted_output.to_str().unwrap(),
        ])
        .assert()
        .success();
    let trusted_html = std::fs::read_to_string(&trusted_output).unwrap();
    assert!(
        trusted_html.contains("<span class=\"badge\">trusted HTML</span>"),
        "{trusted_html}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parse_args_does_not_select_gui_for_version() {
    assert_eq!(parse_args(&["-V".into()]), CliAction::Version);
}

#[test]
fn cli_help_via_assert_cmd() {
    let mut cmd = Command::cargo_bin("markrust").unwrap();
    let output = cmd.arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage"), "{stdout}");
    assert!(stdout.contains("export"), "{stdout}");
    assert!(stdout.contains("--unsafe-html"), "{stdout}");
    assert!(
        stdout.contains("--version") || stdout.contains("-V"),
        "{stdout}"
    );
}

#[test]
fn cli_export_missing_file_errors() {
    let mut cmd = Command::cargo_bin("markrust").unwrap();
    let output = cmd
        .args(["export", "/no/such/markrust-missing.md"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("export failed") || stderr.contains("No such"),
        "{stderr}"
    );

    let code = run_export(&["/no/such/markrust-missing.md".into()]);
    assert_eq!(code, 1);
    assert_eq!(run_export(&[]), 1);
}
