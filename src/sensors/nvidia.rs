// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Nvidia accelerator sensor — sysfs / /proc only.
//!
//! No NVML linkage and no nvidia-smi shelling. We read:
//! - `/sys/class/drm/cardN/device/{vendor,device,resource,numa_node,...}`
//! - `/proc/driver/nvidia/version` (driver string)
//! - `/proc/driver/nvidia/gpus/<bus>/information` (model, UUID, BARs)
//! - `<device>/hwmon/hwmon*/` if present (open driver exposes some).
//!
//! Coverage is `IdentificationOnly` for the closed driver: we publish
//! BAR-derived dedicated VRAM total and metadata, but live util/temp/power
//! are unavailable without NVML. Sensor health stays 1 as long as sysfs
//! reads work.

use std::collections::HashMap;
use std::fs;

use async_trait::async_trait;

use super::common;
use super::{Accelerator, AcceleratorId, AcceleratorSensor, Coverage, MemoryKind, Snapshot, Vendor};

pub struct NvidiaSysfsSensor;

#[async_trait]
impl AcceleratorSensor for NvidiaSysfsSensor {
	fn vendor(&self) -> Vendor {
		Vendor::Nvidia
	}

	async fn enumerate(&self) -> anyhow::Result<Vec<Accelerator>> {
		let proc_models = read_proc_nvidia_models();
		Ok(common::enumerate_for_vendor(Vendor::Nvidia, |drm_index, device_dir| {
			build_accelerator(drm_index, device_dir, &proc_models)
		}))
	}

	async fn snapshot(&self, accel: &Accelerator) -> anyhow::Result<Snapshot> {
		let mut m = Vec::new();

		if let Some(total) = accel.memory_total_bytes {
			m.push(common::measurement(
				"accel.memory.dedicated.total",
				"By",
				"Dedicated VRAM total (from PCI BAR1; sysfs-only path).",
				total as f64,
			));
		}

		// Open-driver hwmon (best effort). nvidia-open exposes some entries;
		// the closed driver does not.
		if let Some(t) = common::hwmon_temperature_celsius(&accel.device_dir) {
			m.push(common::measurement(
				"accel.temperature",
				"Cel",
				"Edge temperature from open-driver hwmon.",
				t,
			));
		}

		m.push(common::sensor_health(&accel.device_dir, Vendor::Nvidia));
		Ok(Snapshot { measurements: m })
	}
}

fn build_accelerator(drm_index: u32, device_dir: std::path::PathBuf, proc_models: &HashMap<String, String>) -> Accelerator {
	let device_id = common::read_hex_u16(&device_dir.join("device")).unwrap_or(0);
	let pci_addr = common::pci_addr_from_device_dir(&device_dir).unwrap_or_default();

	// /proc model lookup keys are case-insensitive bus IDs; normalize once.
	let model = proc_models
		.get(&pci_addr.to_ascii_lowercase())
		.cloned()
		.unwrap_or_else(|| format!("Nvidia device {device_id:#06x}"));

	// Memory total from PCI BAR1: NVIDIA discrete cards expose VRAM as
	// BAR1 (64-bit prefetchable). On a 4090 this is 32 GiB.
	let bar1 = common::read_pci_bars(&device_dir).get(1).copied().unwrap_or(0);
	let memory_total_bytes = (bar1 > 4 * 1024 * 1024).then_some(bar1);
	let memory_kind = if memory_total_bytes.is_some() { MemoryKind::Dedicated } else { MemoryKind::Unknown };

	Accelerator {
		id: AcceleratorId { vendor: Vendor::Nvidia, drm_index, pci_addr },
		model,
		memory_kind,
		memory_total_bytes,
		numa_node: common::read_i32(&device_dir.join("numa_node")),
		local_cpus: common::read_local_cpus(&device_dir),
		iommu_group: None,
		coverage: Coverage::IdentificationOnly,
		fabric_domain: None,
		partitioned: false,
		device_dir,
	}
}

/// Parse `/proc/driver/nvidia/gpus/<bus>/information` for each present GPU
/// and return a map from lowercased PCI address → model name. Used to
/// recover canonical model strings the closed driver doesn't put in sysfs.
fn read_proc_nvidia_models() -> HashMap<String, String> {
	let mut out = HashMap::new();
	let Ok(entries) = fs::read_dir("/proc/driver/nvidia/gpus") else { return out };
	for entry in entries.flatten() {
		let Some(bus_id) = entry.file_name().to_str().map(str::to_ascii_lowercase) else { continue };
		let Ok(text) = fs::read_to_string(entry.path().join("information")) else { continue };
		if let Some(model) = parse_nvidia_info_field(&text, "Model") {
			out.insert(bus_id, model);
		}
	}
	out
}

fn parse_nvidia_info_field(text: &str, key: &str) -> Option<String> {
	text.lines().find_map(|line| {
		let (k, v) = line.split_once(':')?;
		(k.trim() == key).then(|| v.trim().to_string())
	})
}
