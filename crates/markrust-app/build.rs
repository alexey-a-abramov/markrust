// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod build_support;

fn main() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    // Track the product's inputs rather than only this script: changes in a
    // sibling crate also produce a new desktop executable. A no-op cached
    // build keeps the timestamp of the existing binary.
    for path in [
        "build.rs",
        "build_support.rs",
        "Cargo.toml",
        "src",
        "../../Cargo.toml",
        "../../Cargo.lock",
        "../../assets/fonts",
        "tests/ui-review.html",
        "tests/visual-fixtures",
        "../markrust-core/Cargo.toml",
        "../markrust-core/src",
        "../markrust-editor/Cargo.toml",
        "../markrust-editor/src",
        "../markrust/Cargo.toml",
        "../markrust/src",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let source_epoch = std::env::var("SOURCE_DATE_EPOCH").map_or_else(
        |error| match error {
            std::env::VarError::NotPresent => None,
            std::env::VarError::NotUnicode(_) => panic!("SOURCE_DATE_EPOCH must be valid UTF-8"),
        },
        Some,
    );
    let clock_seconds = if source_epoch.is_some() {
        0
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("build clock is before the Unix epoch")
            .as_secs()
    };
    let timestamp = build_support::timestamp_utc(source_epoch.as_deref(), clock_seconds)
        .expect("invalid build timestamp");
    let source = if source_epoch.is_some() {
        "source-date-epoch"
    } else {
        "build-clock"
    };
    println!("cargo:rustc-env=MARKRUST_BUILD_UTC={timestamp}");
    println!("cargo:rustc-env=MARKRUST_BUILD_TIMESTAMP_SOURCE={source}");
}
