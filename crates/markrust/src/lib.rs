// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CLI parsing and non-GUI dispatch for the `markrust` binary.
//!
//! GUI launch is isolated behind [`CliAction::Gui`] so tests never open a window.

use std::path::PathBuf;

use markrust_core::{export_file_to_html, Document};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    Gui { open: Option<PathBuf> },
    Version,
    Help,
    Export { args: Vec<String> },
    Unknown(String),
}

pub fn parse_args(args: &[String]) -> CliAction {
    match args.first().map(String::as_str) {
        None => CliAction::Gui { open: None },
        Some("--gui") => {
            let path = args
                .get(1)
                .filter(|arg| looks_like_open_target(arg))
                .map(PathBuf::from);
            CliAction::Gui { open: path }
        }
        Some("-h") | Some("--help") => CliAction::Help,
        Some("-V") | Some("--version") => CliAction::Version,
        Some("export") => CliAction::Export {
            args: args[1..].to_vec(),
        },
        Some(other) if looks_like_open_target(other) => CliAction::Gui {
            open: Some(PathBuf::from(other)),
        },
        Some(other) => CliAction::Unknown(other.to_string()),
    }
}

pub fn looks_like_open_target(arg: &str) -> bool {
    if arg.starts_with('-') {
        return false;
    }
    let path = PathBuf::from(arg);
    path.exists()
        || path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown" | "txt")
            })
}

pub fn print_help() {
    println!("MarkRust {VERSION} — fast native Markdown workspace");
    println!();
    println!("Usage:");
    println!("  markrust [PATH]                    Launch the desktop editor");
    println!("  markrust --gui [PATH]              Launch the desktop editor");
    println!("  markrust export <file.md> [-o out.html]");
    println!();
    println!("PATH may be a Markdown file or a workspace folder.");
    println!();
    println!("Options:");
    println!("  --gui            Launch the desktop editor (default when no args)");
    println!("  -h, --help       Print help");
    println!("  -V, --version    Print version");
    println!("  -o, --output     HTML output path (export subcommand)");
}

pub fn print_version() {
    println!("markrust {VERSION}");
    let _doc = Document::new("");
}

pub fn run_export(args: &[String]) -> i32 {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-o" | "--output" => {
                output = iter.next().map(PathBuf::from);
            }
            "-h" | "--help" => {
                print_help();
                return 0;
            }
            other if input.is_none() => {
                input = Some(PathBuf::from(other));
            }
            other => {
                eprintln!("Unknown export argument: {other}");
                print_help();
                return 1;
            }
        }
    }
    let Some(input) = input else {
        eprintln!("export requires a Markdown file path");
        print_help();
        return 1;
    };
    match export_file_to_html(&input, output.as_deref()) {
        Ok(path) => {
            println!("{}", path.display());
            0
        }
        Err(err) => {
            eprintln!("export failed: {err}");
            1
        }
    }
}

/// Dispatch CLI args. GUI variants call into `markrust-app` and must not run in tests.
pub fn run(args: &[String]) -> i32 {
    match parse_args(args) {
        CliAction::Gui { open } => {
            if args.first().map(String::as_str) == Some("--gui") || open.is_some() {
                markrust_app::run_gui_with_open(open);
            } else {
                markrust_app::run_gui();
            }
            0
        }
        CliAction::Version => {
            print_version();
            0
        }
        CliAction::Help => {
            print_help();
            0
        }
        CliAction::Export { args } => run_export(&args),
        CliAction::Unknown(other) => {
            eprintln!("Unknown argument: {other}");
            print_help();
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::markdown_to_html_gfm;

    #[test]
    fn version_is_set() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn export_renders_gfm_html() {
        let html = markdown_to_html_gfm("# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<table>"));
        assert!(html.contains("Title"));
    }

    #[test]
    fn open_target_detects_markdown_and_existing_paths() {
        assert!(looks_like_open_target("notes.md"));
        assert!(looks_like_open_target("README.markdown"));
        assert!(!looks_like_open_target("--gui"));
        assert!(!looks_like_open_target("export"));
    }

    #[test]
    fn parse_args_never_implies_gui_for_version_help_export() {
        assert_eq!(parse_args(&["--version".into()]), CliAction::Version);
        assert_eq!(parse_args(&["-V".into()]), CliAction::Version);
        assert_eq!(parse_args(&["--help".into()]), CliAction::Help);
        assert_eq!(parse_args(&["-h".into()]), CliAction::Help);
        assert!(matches!(
            parse_args(&["export".into(), "n.md".into()]),
            CliAction::Export { .. }
        ));
        assert!(matches!(
            parse_args(&["--wat".into()]),
            CliAction::Unknown(_)
        ));
    }
}
