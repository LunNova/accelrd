// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Intel GPU sensor — i915 / Xe sysfs only. Covers integrated graphics
//! (Tiger Lake/Alder Lake/Meteor Lake), Arc discrete cards, and Xe2.
//!
//! All Intel iGPUs are unified-memory: the GPU shares system RAM through
//! GTT. Discrete Arc cards have dedicated VRAM exposed via PCI BAR2
//! (Battlemage onward) or BAR0 (Alchemist).

use async_trait::async_trait;

use super::common;
use super::{Accelerator, AcceleratorId, AcceleratorSensor, Coverage, MemoryKind, Snapshot, Vendor};

const ARC_DISCRETE_VRAM_FLOOR: u64 = 1024 * 1024 * 1024;

pub struct IntelSysfsSensor;

#[async_trait]
impl AcceleratorSensor for IntelSysfsSensor {
	fn vendor(&self) -> Vendor {
		Vendor::Intel
	}

	async fn enumerate(&self) -> anyhow::Result<Vec<Accelerator>> {
		Ok(common::enumerate_for_vendor(Vendor::Intel, build_accelerator))
	}

	async fn snapshot(&self, accel: &Accelerator) -> anyhow::Result<Snapshot> {
		let dir = &accel.device_dir;
		let mut m = Vec::new();

		if let Some(total) = accel.memory_total_bytes {
			m.push(common::measurement(
				"accel.memory.dedicated.total",
				"By",
				"Dedicated VRAM total (from PCI BAR2 on Intel discrete).",
				total as f64,
			));
		}

		// hwmon: i915/Xe drivers expose temp + (Arc only) power.
		for hw in common::hwmon_dirs(dir) {
			if let Some(t) = common::read_u64(&hw.join("temp1_input")) {
				m.push(common::measurement(
					"accel.temperature",
					"Cel",
					"GPU temperature from i915/Xe hwmon.",
					t as f64 / 1000.0,
				));
			}
			if let Some(p) = common::read_u64(&hw.join("power1_input")) {
				m.push(common::measurement(
					"accel.power.usage",
					"W",
					"Package power from i915/Xe hwmon (µW).",
					p as f64 / 1_000_000.0,
				));
			}
		}

		m.push(common::sensor_health(dir, Vendor::Intel));
		Ok(Snapshot { measurements: m })
	}
}

fn build_accelerator(drm_index: u32, device_dir: std::path::PathBuf) -> Accelerator {
	let device_id = common::read_hex_u16(&device_dir.join("device")).unwrap_or(0);
	let pci_addr = common::pci_addr_from_device_dir(&device_dir).unwrap_or_default();

	// Discrete Arc cards have a sizable BAR2 (8/16 GB); iGPUs have no
	// card-local VRAM, so memory_kind = Unified.
	let bar2 = common::read_pci_bars(&device_dir).get(2).copied().unwrap_or(0);
	let (memory_kind, memory_total_bytes) = if bar2 > ARC_DISCRETE_VRAM_FLOOR {
		(MemoryKind::Dedicated, Some(bar2))
	} else {
		(MemoryKind::Unified, None)
	};

	Accelerator {
		id: AcceleratorId { vendor: Vendor::Intel, drm_index, pci_addr },
		model: format!("Intel device {device_id:#06x}"),
		memory_kind,
		memory_total_bytes,
		numa_node: common::read_i32(&device_dir.join("numa_node")),
		local_cpus: common::read_local_cpus(&device_dir),
		iommu_group: None,
		coverage: Coverage::Full,
		fabric_domain: None,
		partitioned: false,
		device_dir,
	}
}
