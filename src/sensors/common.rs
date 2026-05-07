// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Sysfs walking helpers. Read-only, best-effort: missing files return
//! `None` rather than erroring.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::{Accelerator, Measurement, Vendor};

/// Walk `/sys/class/drm/cardN/` for cards that match `vendor` and build an
/// `Accelerator` for each via the vendor-supplied constructor. Every backend's
/// `enumerate()` collapses to one call to this helper.
pub fn enumerate_for_vendor(vendor: Vendor, build: impl Fn(u32, PathBuf) -> Accelerator) -> Vec<Accelerator> {
	drm_cards()
		.into_iter()
		.filter(|(_, dir)| card_vendor(dir) == vendor)
		.map(|(idx, dir)| build(idx, dir))
		.collect()
}

/// Iterate `/sys/class/drm/card<N>/` (just the bare cardN dirs, not the
/// connector subdirs like card0-DP-4) and return `(drm_index, real_device_dir)`
/// for each, sorted by DRM minor. The "real device dir" is what
/// `/sys/class/drm/cardN/device` resolves to under `/sys/devices/...`.
pub fn drm_cards() -> Vec<(u32, PathBuf)> {
	let drm_root = Path::new("/sys/class/drm");
	let Ok(entries) = fs::read_dir(drm_root) else { return Vec::new() };
	let mut out: Vec<(u32, PathBuf)> = entries
		.flatten()
		.filter_map(|entry| {
			// Match bare cardN, not card0-HDMI-A-1, etc.
			let name = entry.file_name().into_string().ok()?;
			let idx: u32 = name.strip_prefix("card")?.parse().ok()?;
			let real = fs::canonicalize(drm_root.join(&name).join("device")).ok()?;
			Some((idx, real))
		})
		.collect();
	out.sort_by_key(|&(idx, _)| idx);
	out
}

pub fn read_hex_u16(path: &Path) -> Option<u16> {
	let s = fs::read_to_string(path).ok()?;
	u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()
}

pub fn read_u64(path: &Path) -> Option<u64> {
	fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn read_i32(path: &Path) -> Option<i32> {
	fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn read_string_first_line(path: &Path) -> Option<String> {
	fs::read_to_string(path).ok().and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
}

/// Parse `/sys/...local_cpulist` ("0-31" or "0-7,16-23") into individual
/// CPU ids.
pub fn parse_cpulist(s: &str) -> Vec<usize> {
	s.split(',')
		.map(str::trim)
		.filter(|piece| !piece.is_empty())
		.flat_map(|piece| match piece.split_once('-') {
			Some((a, b)) => match (a.parse::<usize>(), b.parse::<usize>()) {
				(Ok(a), Ok(b)) => (a..=b).collect::<Vec<_>>(),
				_ => Vec::new(),
			},
			None => piece.parse::<usize>().ok().into_iter().collect(),
		})
		.collect()
}

/// Convenience for reading the per-device `local_cpulist` and parsing it,
/// since every backend does this identically.
pub fn read_local_cpus(device_dir: &Path) -> Vec<usize> {
	read_string_first_line(&device_dir.join("local_cpulist"))
		.map(|s| parse_cpulist(&s))
		.unwrap_or_default()
}

/// Read PCI BAR sizes from `<device>/resource`. Returns 0 for unmapped BARs.
/// Each line is `start end flags` in hex; BAR size = end - start + 1 when
/// start != 0, else 0.
pub fn read_pci_bars(device_dir: &Path) -> Vec<u64> {
	let Ok(s) = fs::read_to_string(device_dir.join("resource")) else { return Vec::new() };
	s.lines()
		.map(|line| {
			let mut parts = line.split_ascii_whitespace().map(parse_hex);
			let start = parts.next().unwrap_or(0);
			let end = parts.next().unwrap_or(0);
			if start == 0 { 0 } else { end - start + 1 }
		})
		.collect()
}

fn parse_hex(s: &str) -> u64 {
	u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

/// Extract PCI address (e.g. `0000:01:00.0`) from a canonicalized device dir like
/// `/sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0`.
pub fn pci_addr_from_device_dir(device_dir: &Path) -> Option<String> {
	device_dir.file_name()?.to_str().filter(|s| s.contains(':')).map(str::to_string)
}

/// Best-effort hwmon discovery: list `<device>/hwmon/hwmon*/` dirs.
pub fn hwmon_dirs(device_dir: &Path) -> Vec<PathBuf> {
	let hw = device_dir.join("hwmon");
	let Ok(entries) = fs::read_dir(&hw) else { return Vec::new() };
	entries
		.flatten()
		.filter(|entry| entry.file_name().to_string_lossy().starts_with("hwmon"))
		.map(|entry| hw.join(entry.file_name()))
		.collect()
}

/// First hwmon temperature reading (in °C) for the device. None if no
/// hwmon dir exposes `temp1_input`.
pub fn hwmon_temperature_celsius(device_dir: &Path) -> Option<f64> {
	hwmon_dirs(device_dir).into_iter().find_map(|hw| read_u64(&hw.join("temp1_input")).map(|t| t as f64 / 1000.0))
}

/// Mark accelerators that share a parent PCI function as partitioned,
/// and additionally flag any AMD card configured for SR-IOV (where
/// `sriov_numvfs > 0`) as partitioned even when the VFs aren't yet
/// bound to DRM card minors. This catches the case where an Instinct
/// is host-mode-configured for vGPU but partitions haven't been
/// instantiated yet.
pub fn mark_partitions(accels: &mut [Accelerator]) {
	let mut counts: HashMap<PathBuf, usize> = HashMap::new();
	for a in accels.iter() {
		if let Some(parent) = a.device_dir.parent() {
			*counts.entry(parent.to_path_buf()).or_default() += 1;
		}
	}
	for a in accels.iter_mut() {
		let shared_parent = a.device_dir.parent().and_then(|p| counts.get(p)).is_some_and(|&c| c > 1);
		// SR-IOV: a non-zero numvfs configuration means the card is
		// partitioned at the PCI level even if VFs aren't bound here.
		let sriov_active = read_u64(&a.device_dir.join("sriov_numvfs")).unwrap_or(0) > 0;
		a.partitioned |= shared_parent || sriov_active;
	}
}

/// Read `sriov_totalvfs` and `sriov_numvfs`. Returns `(total, current)`
/// where `total` is the maximum supported and `current` is what's
/// configured. Used by topology label generation to surface partition
/// capacity.
pub fn sriov_capacity(device_dir: &Path) -> Option<(u64, u64)> {
	let total = read_u64(&device_dir.join("sriov_totalvfs"))?;
	let current = read_u64(&device_dir.join("sriov_numvfs")).unwrap_or(0);
	Some((total, current))
}

/// Build a `Measurement` with no extra attributes — the common case for
/// every per-accelerator gauge we emit.
pub fn measurement(name: &'static str, unit: &'static str, description: &'static str, value: f64) -> Measurement {
	Measurement { name, unit, description, value, attrs: Vec::new() }
}

/// `accel.sensor.health` measurement: 1.0 if the device's PCI vendor ID
/// reads back as the expected vendor, 0.0 otherwise. Universal across
/// backends.
pub fn sensor_health(device_dir: &Path, expected: Vendor) -> Measurement {
	let healthy = card_vendor(device_dir) == expected;
	measurement(
		"accel.sensor.health",
		"1",
		"1 = sensor reads succeeding, 0 = backend broken.",
		if healthy { 1.0 } else { 0.0 },
	)
}

/// Read the PCI vendor ID for a device dir and map it to our `Vendor`
/// enum. Returns `Vendor::Other` if the file is missing or the ID is
/// unknown — backends use this to filter `drm_cards()` to their own.
pub fn card_vendor(device_dir: &Path) -> Vendor {
	read_hex_u16(&device_dir.join("vendor")).map(Vendor::from_pci_id).unwrap_or(Vendor::Other)
}
