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
//! * `host.power.usage` — instantaneous power draw (W) read from any
//!   non-GPU hwmon `power*_input` (CPU package via `amd_energy` / Intel
//!   IPMI / chassis BMCs). GPU power lives on the per-accelerator
//!   `accel.power.usage` series.
//!
//! The probe-readiness preflight checks read these to gate "do we have
//! enough RAM and disk to start a GPU workload" — image pulls, ephemeral
//! storage, model checkpoints all consume both, and a half-full host
//! makes a half-broken probe.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use super::Measurement;

const MEMINFO: &str = "/proc/meminfo";
const MOUNTINFO: &str = "/proc/self/mountinfo";
const HWMON_DIR: &str = "/sys/class/hwmon";

/// Filesystem types we report disk-free for. Anything else (tmpfs,
/// devtmpfs, proc, sys, cgroup, …) is virtual or in-memory and isn't
/// the resource a scheduler decision should hinge on.
const REAL_FILESYSTEMS: &[&str] = &["ext4", "xfs", "btrfs", "zfs", "f2fs", "ext3", "ext2"];

/// Hwmon `name` values that belong to GPUs / storage. We skip these when
/// scanning for host-level power because their `power*_input` (if any) is
/// already attributed via `accel.power.usage` or is irrelevant to fleet
/// power totals.
const SKIP_HWMON_NAMES: &[&str] = &["amdgpu", "i915", "xe", "nouveau", "nvidia", "nvme", "drivetemp"];

pub fn snapshot() -> Vec<Measurement> {
	let mut out = Vec::new();
	out.extend(memory_metrics());
	out.extend(disk_metrics());
	out.extend(power_metrics());
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

/// Walk `/sys/class/hwmon/*` and emit `host.power.usage` for any
/// `power*_input` reading from a non-GPU/non-storage hwmon. The reading
/// is instantaneous (no time-delta math); on Linux this is microwatts
/// per the hwmon spec, so we divide by 1e6 for Watts.
///
/// The likely sources on a typical box: `amd_energy` (per-CCD/package
/// for Zen CPUs), Intel chassis power via IPMI (`ipmi_si`), enterprise
/// BMCs that expose PSU readings. If none of those modules are loaded
/// this returns nothing — fleet power then equals GPU power as before,
/// which is the honest answer.
fn power_metrics() -> Vec<Measurement> {
	let mut out = Vec::new();
	let Ok(entries) = fs::read_dir(HWMON_DIR) else {
		return out;
	};
	for entry in entries.flatten() {
		let dir = entry.path();
		let Some(name) = read_first_line(&dir.join("name")) else {
			continue;
		};
		if SKIP_HWMON_NAMES.contains(&name.as_str()) {
			continue;
		}
		for power_path in power_input_files(&dir) {
			let Some(uw) = read_u64(&power_path) else { continue };
			let watts = uw as f64 / 1_000_000.0;
			let mut attrs = vec![("source", name.clone())];
			let label_path = power_path
				.file_name()
				.and_then(|n| n.to_str())
				.map(|n| dir.join(n.replace("_input", "_label")));
			if let Some(label) = label_path.as_deref().and_then(read_first_line) {
				attrs.push(("rail", label));
			}
			out.push(Measurement {
				name: "host.power.usage",
				unit: "W",
				description: "Instantaneous power draw from a non-GPU hwmon power_input (CPU/chassis/PSU).",
				value: watts,
				attrs,
			});
		}
	}
	out
}

fn power_input_files(dir: &Path) -> Vec<PathBuf> {
	let Ok(entries) = fs::read_dir(dir) else {
		return Vec::new();
	};
	let mut paths: Vec<PathBuf> = entries
		.flatten()
		.map(|e| e.path())
		.filter(|p| {
			p.file_name()
				.and_then(|n| n.to_str())
				.is_some_and(|n| n.starts_with("power") && n.ends_with("_input"))
		})
		.collect();
	paths.sort();
	paths
}

fn read_first_line(p: &Path) -> Option<String> {
	let s = fs::read_to_string(p).ok()?;
	let line = s.lines().next()?.trim().to_string();
	(!line.is_empty()).then_some(line)
}

fn read_u64(p: &Path) -> Option<u64> {
	read_first_line(p)?.parse().ok()
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
