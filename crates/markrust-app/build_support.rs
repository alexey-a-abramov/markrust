// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Produce a portable, seconds-precision UTC build identity. An explicitly
/// supplied reproducible epoch must be valid; never silently replace it.
pub fn timestamp_utc(source_epoch: Option<&str>, clock_seconds: u64) -> Result<String, String> {
    let seconds = match source_epoch {
        Some(value) => {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("SOURCE_DATE_EPOCH must contain nonnegative integer seconds".into());
            }
            value
                .parse::<u64>()
                .map_err(|_| "SOURCE_DATE_EPOCH is too large")?
        }
        None => clock_seconds,
    };
    let seconds = i64::try_from(seconds).map_err(|_| "build timestamp is too large")?;
    OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|_| "build timestamp is outside the supported date range")?
        .format(&Rfc3339)
        .map_err(|_| "build timestamp cannot be represented as RFC 3339 UTC".into())
}
