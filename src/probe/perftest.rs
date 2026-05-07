// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! `ib_send_bw` invocation + summary-line parser. perftest's output
//! shape is stable enough across versions that we can scan for the
//! header row containing `#bytes`, `#iterations`, `MsgRate` and treat
//! the next non-separator line as the summary. We always pass
//! `--report_gbits` so the bandwidth columns are unambiguously Gb/s.

use std::process::Command;

use anyhow::Context;

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
	pub bytes: u64,
	pub iterations: u64,
	pub bw_peak_gbps: f64,
	pub bw_avg_gbps: f64,
	pub msg_rate_mpps: f64,
}

/// Build an `ib_send_bw` Command for either side of a pair. `server`
/// is `None` for the listener and `Some(host)` for the client.
///
/// Flags:
///  - `-F`: don't fail if cpufreq governor isn't `performance` (containers
///    typically don't expose it; test still runs fine).
///  - `--report_gbits`: bandwidth columns in Gb/sec instead of MB/sec, so
///    parsing is unit-stable across distros' perftest versions.
///  - `-d <dev>`: pick the RDMA device explicitly. Without it perftest
///    picks an "active" port which is non-deterministic on multi-port hosts.
///  - `-p <port>`: TCP port for the QP exchange. Both sides must agree.
///  - `--duration <s>`: run for that many seconds rather than a fixed
///    iteration count. Iteration-based mode would let a slow link stretch
///    test runtime past our pod-watch timeout.
pub fn build_cmd(server: Option<&str>, device: &str, port: u16, duration_secs: u64) -> Command {
	let mut cmd = Command::new("ib_send_bw");
	cmd.arg("-F")
		.arg("--report_gbits")
		.arg("-d")
		.arg(device)
		.arg("-p")
		.arg(port.to_string())
		.arg("--duration")
		.arg(duration_secs.to_string());
	if let Some(s) = server {
		cmd.arg(s);
	}
	cmd
}

/// Locate and parse the perftest summary row in `stdout`. Returns
/// `None` when the header isn't present (perftest crashed early) or
/// the data row is malformed.
pub fn parse(stdout: &str) -> Option<Summary> {
	let lines: Vec<&str> = stdout.lines().collect();
	for (i, line) in lines.iter().enumerate() {
		if !is_header(line) {
			continue;
		}
		for next in &lines[i + 1..] {
			let trimmed = next.trim();
			if trimmed.is_empty() || trimmed.starts_with("---") {
				continue;
			}
			if let Some(s) = parse_row(trimmed) {
				return Some(s);
			}
			break;
		}
	}
	None
}

fn is_header(line: &str) -> bool {
	line.contains("#bytes") && line.contains("#iterations") && line.contains("MsgRate")
}

fn parse_row(row: &str) -> Option<Summary> {
	let parts: Vec<&str> = row.split_whitespace().collect();
	if parts.len() < 5 {
		return None;
	}
	Some(Summary {
		bytes: parts[0].parse().ok()?,
		iterations: parts[1].parse().ok()?,
		bw_peak_gbps: parts[2].parse().ok()?,
		bw_avg_gbps: parts[3].parse().ok()?,
		msg_rate_mpps: parts[4].parse().ok()?,
	})
}

/// Convenience for the wrapper: combines `parse` + a clear error.
pub fn parse_required(stdout: &str) -> anyhow::Result<Summary> {
	parse(stdout).context("perftest output had no parseable summary row")
}

#[cfg(test)]
mod tests {
	use super::*;

	const SAMPLE_GBITS: &str = "\
---------------------------------------------------------------------------------------
                    Send BW Test
 Dual-port       : OFF		Device         : mlx5_0
 Number of qps   : 1		Transport type : IB
 Connection type : RC		Using SRQ      : OFF
 PCIe relax order: ON
 ibv_wr* API     : OFF
 TX depth        : 128
 CQ Moderation   : 100 Mtu             : 4096[B]
 Link type       : Ethernet GID index       : 3
 Max inline data : 0[B]
 rdma_cm QPs	 : OFF
 Data ex. method : Ethernet
---------------------------------------------------------------------------------------
 local address: LID 0000 QPN 0x0050 PSN 0xa6e8e1
 GID: 00:00:00:00:00:00:00:00:00:00:255:255:10:00:00:01
 remote address: LID 0000 QPN 0x0050 PSN 0xa6e8e1
 GID: 00:00:00:00:00:00:00:00:00:00:255:255:10:00:00:02
---------------------------------------------------------------------------------------
 #bytes     #iterations    BW peak[Gb/sec]    BW average[Gb/sec]   MsgRate[Mpps]
 65536      5000             89.60              89.40                0.179173
---------------------------------------------------------------------------------------
";

	const SAMPLE_MBPS: &str = "\
---------------------------------------------------------------------------------------
 #bytes     #iterations    BW peak[MB/sec]    BW average[MB/sec]   MsgRate[Mpps]
 65536      1000             11200.45         11198.32             0.179173
---------------------------------------------------------------------------------------
";

	#[test]
	fn parses_gbits_summary() {
		let s = parse(SAMPLE_GBITS).expect("parse");
		assert_eq!(s.bytes, 65536);
		assert_eq!(s.iterations, 5000);
		assert!((s.bw_peak_gbps - 89.60).abs() < 1e-6);
		assert!((s.bw_avg_gbps - 89.40).abs() < 1e-6);
		assert!((s.msg_rate_mpps - 0.179173).abs() < 1e-9);
	}

	#[test]
	fn parses_mbps_summary_into_floats() {
		// Even when --report_gbits wasn't passed, the parser still
		// extracts the floats — we'd just be misinterpreting the unit.
		// Defensive: callers should always pass --report_gbits.
		let s = parse(SAMPLE_MBPS).expect("parse");
		assert_eq!(s.bytes, 65536);
		assert!((s.bw_peak_gbps - 11200.45).abs() < 1e-3);
	}

	#[test]
	fn returns_none_on_malformed_input() {
		assert!(parse("nothing useful here").is_none());
		assert!(parse("").is_none());
	}

	#[test]
	fn build_cmd_client_has_server_arg() {
		let cmd = build_cmd(Some("10.0.0.5"), "mlx5_0", 18515, 30);
		let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
		assert!(args.iter().any(|a| a.to_string_lossy() == "10.0.0.5"));
		assert!(args.iter().any(|a| a.to_string_lossy() == "mlx5_0"));
		assert!(args.iter().any(|a| a.to_string_lossy() == "--report_gbits"));
		assert!(args.iter().any(|a| a.to_string_lossy() == "30"));
	}

	#[test]
	fn build_cmd_server_omits_server_arg() {
		let cmd = build_cmd(None, "mlx5_0", 18515, 30);
		let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
		// No positional "host" — the last arg is the duration value.
		assert_eq!(args.last().map(String::as_str), Some("30"));
	}
}
