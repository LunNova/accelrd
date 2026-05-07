// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Parse the JSON record line that the probe pod prints on stdout.
//! Mirrors `crate::probe::Record` field-for-field. Decoupled into its
//! own type rather than importing because the prober may eventually
//! consume probe results from older agent versions, and a separate
//! deserialization target is the easiest forward-compat seam.

use anyhow::{Context, anyhow};
use serde::Deserialize;

/// Deserialization target for the probe pod's stdout JSON line. Many
/// fields are part of the public contract but not directly read by the
/// prober — they're surfaced via OTLP / annotations and otherwise
/// ignored. `allow(dead_code)` because dropping them would silently
/// shrink the schema.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ProbeRecord {
	pub side: String,
	pub ok: bool,
	pub server: Option<String>,
	pub device: String,
	pub bytes: Option<u64>,
	pub bw_peak_gbps: Option<f64>,
	pub bw_avg_gbps: Option<f64>,
	pub msg_rate_mpps: Option<f64>,
	pub duration_s: f64,
	pub message: Option<String>,
	pub src_rack: Option<String>,
	pub partner_node: Option<String>,
}

pub fn parse(stdout: &str) -> anyhow::Result<ProbeRecord> {
	for line in stdout.lines() {
		let trimmed = line.trim();
		if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
			continue;
		}
		if let Ok(rec) = serde_json::from_str::<ProbeRecord>(trimmed) {
			return Ok(rec);
		}
	}
	Err(anyhow!(
		"no parseable JSON record in probe stdout (probe may have crashed before printing)"
	))
	.context("parse probe stdout")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_ok_record() {
		let log = "\
[2026-05-07T12:00:00Z INFO ] probe starting
{\"side\":\"client\",\"ok\":true,\"server\":\"10.0.0.5\",\"device\":\"mlx5_0\",\"bytes\":65536,\"bw_peak_gbps\":89.6,\"bw_avg_gbps\":89.4,\"msg_rate_mpps\":0.179,\"duration_s\":30.1,\"message\":null,\"src_rack\":\"r1\",\"partner_node\":\"node-b\"}
[2026-05-07T12:00:30Z INFO ] probe complete
";
		let r = parse(log).unwrap();
		assert!(r.ok);
		assert_eq!(r.device, "mlx5_0");
		assert!((r.bw_avg_gbps.unwrap() - 89.4).abs() < 1e-6);
	}

	#[test]
	fn parses_failure_record() {
		let log = "{\"side\":\"client\",\"ok\":false,\"server\":\"10.0.0.5\",\"device\":\"mlx5_0\",\"bytes\":null,\"bw_peak_gbps\":null,\"bw_avg_gbps\":null,\"msg_rate_mpps\":null,\"duration_s\":1.0,\"message\":\"server unreachable\",\"src_rack\":null,\"partner_node\":null}";
		let r = parse(log).unwrap();
		assert!(!r.ok);
		assert_eq!(r.message.as_deref(), Some("server unreachable"));
	}

	#[test]
	fn errors_when_no_json_line() {
		assert!(parse("nothing here\nor here\n").is_err());
	}
}
