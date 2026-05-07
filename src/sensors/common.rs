// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Sysfs walking helpers. Read-only, best-effort: missing files return
//! `None` rather than erroring.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::Accelerator;

/// Iterate `/sys/class/drm/card<N>/` (just the bare cardN dirs, not the
/// connector subdirs like card0-DP-4) and return `(drm_index, real_device_dir)`
/// for each. The "real device dir" is what `/sys/class/drm/cardN/device`
/// resolves to under `/sys/devices/...`.
pub fn drm_cards() -> Vec<(u32, PathBuf)> {
	let mut out = Vec::new();
	let drm_root = Path::new("/sys/class/drm");
	let Ok(entries) = fs::read_dir(drm_root) else { return out };
	for entry in entries.flatten() {
		let name = entry.file_name();
		let Some(name) = name.to_str() else { continue };
		// Match bare cardN, not card0-HDMI-A-1, etc.
		let Some(idx) = name.strip_prefix("card") else { continue };
		if !idx.chars().all(|c| c.is_ascii_digit()) {
			continue;
		}
		let Ok(idx) = idx.parse::<u32>() else { continue };
		let device_link = drm_root.join(name).join("device");
		let Ok(real) = fs::canonicalize(&device_link) else { continue };
		out.push((idx, real));
	}
	out.sort_by_key(|(idx, _)| *idx);
	out
}

pub fn read_hex_u16(path: &Path) -> Option<u16> {
	let s = fs::read_to_string(path).ok()?;
	let s = s.trim().trim_start_matches("0x");
	u16::from_str_radix(s, 16).ok()
}

pub fn read_u64(path: &Path) -> Option<u64> {
	fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn read_i32(path: &Path) -> Option<i32> {
	fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn read_string(path: &Path) -> Option<String> {
	fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn read_string_first_line(path: &Path) -> Option<String> {
	fs::read_to_string(path).ok().and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
}

/// Parse `/sys/...local_cpulist` ("0-31" or "0-7,16-23") into individual
/// CPU ids.
pub fn parse_cpulist(s: &str) -> Vec<usize> {
	let mut out = Vec::new();
	for piece in s.split(',') {
		let piece = piece.trim();
		if piece.is_empty() {
			continue;
		}
		match piece.split_once('-') {
			Some((a, b)) => {
				let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) else { continue };
				out.extend(a..=b);
			}
			None => {
				if let Ok(n) = piece.parse::<usize>() {
					out.push(n);
				}
			}
		}
	}
	out
}

/// Read PCI BAR sizes from `<device>/resource`. Returns 0 for unmapped BARs.
/// Each line is `start end flags` in hex; BAR size = end - start + 1 when
/// start != 0, else 0.
pub fn read_pci_bars(device_dir: &Path) -> Vec<u64> {
	let path = device_dir.join("resource");
	let Ok(s) = fs::read_to_string(&path) else { return vec![] };
	let mut out = Vec::new();
	for line in s.lines() {
		let mut parts = line.split_ascii_whitespace();
		let start = parts.next().unwrap_or("0");
		let end = parts.next().unwrap_or("0");
		let start = u64::from_str_radix(start.trim_start_matches("0x"), 16).unwrap_or(0);
		let end = u64::from_str_radix(end.trim_start_matches("0x"), 16).unwrap_or(0);
		if start == 0 { out.push(0) } else { out.push(end - start + 1) }
	}
	out
}

/// Extract PCI address (e.g. `0000:01:00.0`) from a canonicalized device dir like
/// `/sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0`.
pub fn pci_addr_from_device_dir(device_dir: &Path) -> Option<String> {
	device_dir.file_name().and_then(|s| s.to_str()).map(|s| s.to_string()).filter(|s| s.contains(':'))
}

/// Best-effort hwmon discovery: list `<device>/hwmon/hwmon*/` dirs.
pub fn hwmon_dirs(device_dir: &Path) -> Vec<PathBuf> {
	let hw = device_dir.join("hwmon");
	let Ok(entries) = fs::read_dir(&hw) else { return vec![] };
	let mut out = Vec::new();
	for entry in entries.flatten() {
		let name = entry.file_name();
		if name.to_string_lossy().starts_with("hwmon") {
			out.push(hw.join(name));
		}
	}
	out
}

/// Mark accelerators that share a parent PCI function as partitioned.
/// Useful for MIG-style splits where one physical accelerator presents
/// multiple DRM minor numbers.
pub fn mark_partitions(accels: &mut [Accelerator]) {
	let mut counts: HashMap<PathBuf, usize> = HashMap::new();
	for a in accels.iter() {
		if let Some(parent) = a.device_dir.parent() {
			*counts.entry(parent.to_path_buf()).or_default() += 1;
		}
	}
	for a in accels.iter_mut() {
		if let Some(parent) = a.device_dir.parent()
			&& counts.get(parent).copied().unwrap_or(0) > 1
		{
			a.partitioned = true;
		}
	}
}

