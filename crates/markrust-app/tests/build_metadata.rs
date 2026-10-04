// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[path = "../build_support.rs"]
mod build_support;

#[test]
fn epoch_zero_is_not_replaced_by_the_build_clock() {
    assert_eq!(
        build_support::timestamp_utc(Some("0"), 123).unwrap(),
        "1970-01-01T00:00:00Z"
    );
}

#[test]
fn clock_and_reproducible_inputs_have_the_same_utc_format() {
    assert_eq!(
        build_support::timestamp_utc(None, 1_709_251_199).unwrap(),
        "2024-02-29T23:59:59Z"
    );
    assert_eq!(
        build_support::timestamp_utc(Some("1709251200"), 123).unwrap(),
        "2024-03-01T00:00:00Z"
    );
}

#[test]
fn invalid_or_overflowing_reproducible_epochs_fail() {
    for epoch in [
        "",
        "-1",
        "+1",
        " 1",
        "1\n",
        "oops",
        "18446744073709551616",
        "253402300800",
    ] {
        assert!(
            build_support::timestamp_utc(Some(epoch), 0).is_err(),
            "{epoch:?}"
        );
    }
    assert!(build_support::timestamp_utc(None, u64::MAX).is_err());
}
