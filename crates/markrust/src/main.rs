// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::path::PathBuf;

use markrust_core::{export_file_to_html, Document};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!("MarkRust {VERSION} — fast native Markdown workspace");
    println!();
    println!("Usage:");
    println!("  markrust [OPTIONS]                 Launch the desktop editor");
    println!("  markrust export <file.md> [-o out.html]");
    println!();
    println!("Options:");
    println!("  --gui            Launch the desktop editor (default when no args)");
    println!("  -h, --help       Print help");
    println!("  -V, --version    Print version");
    println!("  -o, --output     HTML output path (export subcommand)");
}

fn print_version() {
    println!("markrust {VERSION}");
    let _doc = Document::new("");
}

fn run_export(args: &[String]) -> i32 {
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

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--gui") => markrust_app::run_gui(),
        Some("-h") | Some("--help") => print_help(),
        Some("-V") | Some("--version") => print_version(),
        Some("export") => {
            args.remove(0);
            std::process::exit(run_export(&args));
        }
        Some(other) => {
            eprintln!("Unknown argument: {other}");
            print_help();
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use markrust_core::markdown_to_html_gfm;

    #[test]
    fn version_is_set() {
        assert_eq!(super::VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn export_renders_gfm_html() {
        let html = markdown_to_html_gfm("# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert!(html.contains("<table>"));
        assert!(html.contains("Title"));
    }
}
