// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! RFC3339 formatting without pulling chrono. Used by both the daemon's
//! preflight log lines and the prober's annotation timestamps.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_rfc3339() -> String {
	let secs = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs();
	rfc3339_at(secs as i64)
}

pub fn rfc3339_at(unix_secs: i64) -> String {
	let s = unix_secs.rem_euclid(60);
	let m = (unix_secs.div_euclid(60)).rem_euclid(60);
	let h = (unix_secs.div_euclid(3600)).rem_euclid(24);
	let (y, mo, d) = days_to_ymd(unix_secs.div_euclid(86_400));
	format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

pub fn now_minus_secs() -> impl FnOnce(u64) -> String {
	let now = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs() as i64;
	move |secs| rfc3339_at(now - secs as i64)
}

/// Howard Hinnant's date algorithm — `days_since_epoch` is 1970-01-01 == 0.
/// See <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>.
fn days_to_ymd(days_since_epoch: i64) -> (i64, u32, u32) {
	let z = days_since_epoch + 719_468;
	let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
	let doe = (z - era * 146_097) as u64;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let y = yoe as i64 + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let m = if mp < 10 { mp + 3 } else { mp - 9 };
	let y = if m <= 2 { y + 1 } else { y };
	(y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn epoch() {
		assert_eq!(rfc3339_at(0), "1970-01-01T00:00:00Z");
	}

	#[test]
	fn known_date_2026() {
		// 2026-05-07 12:34:56 UTC = 1778157296 (verified: `date -u -d ...`)
		assert_eq!(rfc3339_at(1_778_157_296), "2026-05-07T12:34:56Z");
	}

	#[test]
	fn lexicographic_order() {
		assert!(rfc3339_at(1_000_000_000) < rfc3339_at(1_500_000_000));
	}
}
