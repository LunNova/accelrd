// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Host-level sensors. Not per-accelerator — these report node-wide
//! resource availability that schedulers need before placing GPU jobs:
//!
//! * `host.memory.total_bytes` — `MemTotal` from /proc/meminfo.
//! * `host.memory.available_bytes` — `MemAvailable` (the kernel's estimate
//!   of allocatable pages, accounting for reclaimable cache).
//! * `host.memory.swap.total_bytes` / `host.memory.swap.used_bytes`.
//! * `host.disk.free_bytes` — per real (non-virtual) mount, statvfs.
//! * `host.disk.total_bytes` — per mount, statvfs.
//!
//! The probe-readiness preflight checks read these to gate "do we have
//! enough RAM and disk to start a GPU workload" — image pulls, ephemeral
//! storage, model checkpoints all consume both, and a half-full host
//! makes a half-broken probe.

use std::collections::BTreeMap;
use std::ffi::CString;

use super::Measurement;

const MEMINFO: &str = "/proc/meminfo";
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// Filesystem types we report disk-free for. Anything else (tmpfs,
/// devtmpfs, proc, sys, cgroup, …) is virtual or in-memory and isn't
/// the resource a scheduler decision should hinge on.
const REAL_FILESYSTEMS: &[&str] = &["ext4", "xfs", "btrfs", "zfs", "f2fs", "ext3", "ext2"];

pub fn snapshot() -> Vec<Measurement> {
	let mut out = Vec::new();
	out.extend(memory_metrics());
	out.extend(disk_metrics());
	out
}

fn memory_metrics() -> Vec<Measurement> {
	let map = parse_meminfo();
	let mut out = Vec::new();
	if let Some(v) = map.get("MemTotal") {
		out.push(Measurement {
			name: "host.memory.total_bytes",
			unit: "By",
			description: "MemTotal from /proc/meminfo — usable RAM as the kernel sees it.",
			value: *v as f64,
			attrs: Vec::new(),
		});
	}
	if let Some(v) = map.get("MemAvailable") {
		// MemAvailable is the right metric for scheduler decisions: it
		// accounts for reclaimable page cache, unlike MemFree which
		// underreports usable memory on a busy host.
		out.push(Measurement {
			name: "host.memory.available_bytes",
			unit: "By",
			description: "MemAvailable from /proc/meminfo — what userspace can actually allocate.",
			value: *v as f64,
			attrs: Vec::new(),
		});
	}
	if let (Some(swap_total), Some(swap_free)) = (map.get("SwapTotal"), map.get("SwapFree")) {
		out.push(Measurement {
			name: "host.memory.swap.total_bytes",
			unit: "By",
			description: "SwapTotal from /proc/meminfo.",
			value: *swap_total as f64,
			attrs: Vec::new(),
		});
		let used = swap_total.saturating_sub(*swap_free);
		out.push(Measurement {
			name: "host.memory.swap.used_bytes",
			unit: "By",
			description: "SwapTotal - SwapFree.",
			value: used as f64,
			attrs: Vec::new(),
		});
	}
	out
}

/// Returns `(total_bytes, free_bytes)` per real-filesystem mount point.
/// The free-bytes value uses `f_bavail`, not `f_bfree` — bavail accounts
/// for the kernel's reservation for root, which is what an unprivileged
/// scheduler/job allocator would actually see.
fn disk_metrics() -> Vec<Measurement> {
	let mut out = Vec::new();
	for mount in real_mounts() {
		let Some((total, free)) = statvfs_pair(&mount) else {
			continue;
		};
		out.push(Measurement {
			name: "host.disk.total_bytes",
			unit: "By",
			description: "Total bytes on a real (non-virtual) filesystem.",
			value: total as f64,
			attrs: vec![("mount", mount.clone())],
		});
		out.push(Measurement {
			name: "host.disk.free_bytes",
			unit: "By",
			description: "Free bytes available to non-root on a real filesystem.",
			value: free as f64,
			attrs: vec![("mount", mount)],
		});
	}
	out
}

fn parse_meminfo() -> BTreeMap<String, u64> {
	let Ok(s) = std::fs::read_to_string(MEMINFO) else {
		return BTreeMap::new();
	};
	parse_meminfo_str(&s)
}

fn parse_meminfo_str(content: &str) -> BTreeMap<String, u64> {
	let mut out = BTreeMap::new();
	for line in content.lines() {
		// "MemTotal:       263870124 kB"
		let Some((key, rest)) = line.split_once(':') else {
			continue;
		};
		let mut parts = rest.split_whitespace();
		let Some(num) = parts.next() else { continue };
		let Ok(num) = num.parse::<u64>() else { continue };
		let unit = parts.next().unwrap_or("");
		let bytes = match unit {
			"kB" | "KB" => num.saturating_mul(1024),
			"MB" => num.saturating_mul(1024 * 1024),
			"" => num,
			_ => num,
		};
		out.insert(key.trim().to_string(), bytes);
	}
	out
}

fn real_mounts() -> Vec<String> {
	let Ok(s) = std::fs::read_to_string(MOUNTINFO) else {
		return Vec::new();
	};
	parse_mountinfo(&s)
}

/// /proc/self/mountinfo format (one line):
///   36 35 98:0 /mnt1 /mnt parent shared:1 - ext4 /dev/root rw,errors=continue
/// The fs type follows the standalone `-` separator. The fifth field is
/// the mount point.
fn parse_mountinfo(content: &str) -> Vec<String> {
	let mut out: Vec<String> = Vec::new();
	let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
	for line in content.lines() {
		let parts: Vec<&str> = line.split_whitespace().collect();
		// Find separator `-` then read the next token (fs type).
		let Some(sep_idx) = parts.iter().position(|t| *t == "-") else {
			continue;
		};
		let fs_type = parts.get(sep_idx + 1).copied().unwrap_or("");
		if !REAL_FILESYSTEMS.contains(&fs_type) {
			continue;
		}
		let Some(mount_point) = parts.get(4) else { continue };
		// Same backing fs may be mounted at multiple points (bind mounts);
		// dedupe by mount path so we don't double-count.
		if seen.insert(mount_point.to_string()) {
			out.push(mount_point.to_string());
		}
	}
	out
}

/// Returns `(total_bytes, free_bytes_available_to_non_root)`.
fn statvfs_pair(path: &str) -> Option<(u64, u64)> {
	let cpath = CString::new(path).ok()?;
	let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
	let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut buf) };
	if rc != 0 {
		return None;
	}
	let frsize = buf.f_frsize as u64;
	let total = (buf.f_blocks as u64).saturating_mul(frsize);
	let free = (buf.f_bavail as u64).saturating_mul(frsize);
	Some((total, free))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_meminfo_kb_unit() {
		let s = "MemTotal:       263870124 kB\nMemAvailable:   214901168 kB\nHugepagesize:       2048 kB\n";
		let m = parse_meminfo_str(s);
		assert_eq!(m["MemTotal"], 263_870_124u64 * 1024);
		assert_eq!(m["MemAvailable"], 214_901_168u64 * 1024);
	}

	#[test]
	fn skips_malformed_meminfo_lines() {
		let m = parse_meminfo_str("MemTotal:\nbogus line\n");
		assert!(!m.contains_key("MemTotal"));
	}

	#[test]
	fn parses_mountinfo_keeps_only_real_fs() {
		let s = "\
26 1 254:0 / / rw - btrfs /dev/nvme1n1p2 rw
27 26 0:5 / /dev rw - devtmpfs devtmpfs rw
28 26 254:0 /nix /nix rw - btrfs /dev/nvme1n1p2 rw
29 26 0:6 / /sys rw - sysfs sysfs rw
30 26 0:7 /var /var rw - tmpfs tmpfs rw
";
		let mounts = parse_mountinfo(s);
		// btrfs / and /nix are real; tmpfs/devtmpfs/sysfs are virtual.
		assert_eq!(mounts, vec!["/", "/nix"]);
	}
}
