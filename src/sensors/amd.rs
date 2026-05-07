// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! AMD GPU sysfs sensor backend. Reads everything from
//! `/sys/class/drm/cardN/device/` (mem_info_*, gpu_busy_percent, hwmon).
//! Works equally well for discrete Radeon, integrated Raphael/Phoenix
//! iGPUs, and Instinct accelerators.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::common;
use super::{Accelerator, AcceleratorId, AcceleratorSensor, Coverage, Measurement, MemoryKind, Snapshot, Vendor};

/// `(sysfs_filename, metric_name, description)` for each amdgpu memory gauge.
const MEMORY_METRICS: &[(&str, &str, &str)] = &[
	("mem_info_vram_total", "accel.memory.vram.total", "Dedicated VRAM size reported by amdgpu (small on iGPUs)."),
	("mem_info_vram_used", "accel.memory.vram.used", "Used VRAM bytes."),
	("mem_info_gtt_total", "accel.memory.gtt.total", "GTT (host aperture) size — this is the UMA carrier on iGPUs."),
	("mem_info_gtt_used", "accel.memory.gtt.used", "Used GTT bytes."),
	(
		"mem_info_visible_vram_total",
		"accel.memory.visible_vram.total",
		"CPU-visible portion of VRAM (BAR1/Resizable-BAR aperture).",
	),
];

pub struct AmdSysfsSensor;

#[async_trait]
impl AcceleratorSensor for AmdSysfsSensor {
	fn vendor(&self) -> Vendor {
		Vendor::Amd
	}

	async fn enumerate(&self) -> anyhow::Result<Vec<Accelerator>> {
		Ok(common::enumerate_for_vendor(Vendor::Amd, build_accelerator))
	}

	async fn snapshot(&self, accel: &Accelerator) -> anyhow::Result<Snapshot> {
		let dir = &accel.device_dir;
		let mut m = Vec::new();

		for (file, name, desc) in MEMORY_METRICS {
			if let Some(v) = common::read_u64(&dir.join(file)) {
				m.push(common::measurement(name, "By", desc, v as f64));
			}
		}
		if let Some(v) = common::read_u64(&dir.join("gpu_busy_percent")) {
			m.push(common::measurement(
				"accel.utilization",
				"1",
				"Approximate GPU busy fraction (gpu_busy_percent / 100).",
				v as f64 / 100.0,
			));
		}

		// hwmon: temp1_input is mC, power1_average is µW.
		for hw in common::hwmon_dirs(dir) {
			if let Some(t) = common::read_u64(&hw.join("temp1_input")) {
				m.push(Measurement {
					name: "accel.temperature",
					unit: "Cel",
					description: "Edge or junction temperature from hwmon temp1.",
					value: t as f64 / 1000.0,
					attrs: vec![("sensor", hwmon_label(&hw, "temp1"))],
				});
			}
			if let Some(p) = common::read_u64(&hw.join("power1_average")) {
				m.push(common::measurement(
					"accel.power.usage",
					"W",
					"Average board power from hwmon power1_average.",
					p as f64 / 1_000_000.0,
				));
			}
		}

		m.push(common::sensor_health(dir, Vendor::Amd));
		Ok(Snapshot { measurements: m })
	}
}

fn build_accelerator(drm_index: u32, device_dir: PathBuf) -> Accelerator {
	let device_id = common::read_hex_u16(&device_dir.join("device")).unwrap_or(0);
	let pci_addr = common::pci_addr_from_device_dir(&device_dir).unwrap_or_default();
	let (memory_kind, memory_total_bytes) = classify_memory(&device_dir);

	let iommu_group = std::fs::read_link(device_dir.join("iommu_group"))
		.ok()
		.and_then(|p| p.file_name()?.to_str()?.parse::<u32>().ok());

	Accelerator {
		id: AcceleratorId { vendor: Vendor::Amd, drm_index, pci_addr },
		model: amd_model_name(device_id, &device_dir),
		memory_kind,
		memory_total_bytes,
		numa_node: common::read_i32(&device_dir.join("numa_node")),
		local_cpus: common::read_local_cpus(&device_dir),
		iommu_group,
		coverage: Coverage::Full,
		fabric_domain: None,
		partitioned: false,
		device_dir,
	}
}

/// UMA rule: a card whose host-aperture (GTT) dwarfs its dedicated VRAM is
/// presenting an iGPU-style unified memory. For unified, total = GTT (the
/// host aperture is the relevant upper bound). For dedicated, total = VRAM.
/// This matches how a scheduler should reason about "how much can I allocate".
fn classify_memory(device_dir: &Path) -> (MemoryKind, Option<u64>) {
	let vram_total = common::read_u64(&device_dir.join("mem_info_vram_total")).unwrap_or(0);
	let gtt_total = common::read_u64(&device_dir.join("mem_info_gtt_total")).unwrap_or(0);
	if gtt_total > vram_total.saturating_mul(4) {
		(MemoryKind::Unified, Some(gtt_total))
	} else if vram_total > 0 {
		(MemoryKind::Dedicated, Some(vram_total))
	} else {
		(MemoryKind::Unknown, None)
	}
}

fn amd_model_name(device_id: u16, device_dir: &Path) -> String {
	// Best-effort: amdgpu doesn't expose a friendly product string in sysfs.
	// `product_name` exists on some platforms; otherwise fall back to a hex
	// device ID.
	common::read_string_first_line(&device_dir.join("product_name"))
		.filter(|s| !s.is_empty())
		.unwrap_or_else(|| format!("AMD device {device_id:#06x}"))
}

fn hwmon_label(hw: &Path, prefix: &str) -> String {
	common::read_string_first_line(&hw.join(format!("{prefix}_label"))).unwrap_or_else(|| prefix.to_string())
}
