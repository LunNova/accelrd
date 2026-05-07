// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! AMD GPU sysfs sensor backend. Reads everything from
//! `/sys/class/drm/cardN/device/` (mem_info_*, gpu_busy_percent, hwmon).
//! Works equally well for discrete Radeon, integrated Raphael/Phoenix
//! iGPUs, and Instinct accelerators.

use std::path::Path;

use async_trait::async_trait;

use super::common;
use super::{Accelerator, AcceleratorId, AcceleratorSensor, Coverage, Measurement, MemoryKind, Snapshot, Vendor};

pub struct AmdSysfsSensor;

#[async_trait]
impl AcceleratorSensor for AmdSysfsSensor {
	fn vendor(&self) -> Vendor {
		Vendor::Amd
	}

	async fn enumerate(&self) -> anyhow::Result<Vec<Accelerator>> {
		let mut out = Vec::new();
		for (drm_index, device_dir) in common::drm_cards() {
			let vendor_id = common::read_hex_u16(&device_dir.join("vendor")).unwrap_or(0);
			if Vendor::from_pci_id(vendor_id) != Vendor::Amd {
				continue;
			}
			let device_id = common::read_hex_u16(&device_dir.join("device")).unwrap_or(0);
			let model = amd_model_name(device_id, &device_dir);
			let pci_addr = common::pci_addr_from_device_dir(&device_dir).unwrap_or_default();

			let vram_total = common::read_u64(&device_dir.join("mem_info_vram_total")).unwrap_or(0);
			let gtt_total = common::read_u64(&device_dir.join("mem_info_gtt_total")).unwrap_or(0);
			// UMA rule: a card whose host-aperture (GTT) dwarfs its dedicated
			// VRAM is presenting an iGPU-style unified memory.
			let memory_kind = if gtt_total > vram_total.saturating_mul(4) {
				MemoryKind::Unified
			} else if vram_total > 0 {
				MemoryKind::Dedicated
			} else {
				MemoryKind::Unknown
			};
			// For unified, total = GTT (it's the host aperture, the relevant
			// upper bound). For dedicated, total = VRAM. This matches how a
			// scheduler should reason about "how much can I allocate".
			let memory_total_bytes = match memory_kind {
				MemoryKind::Unified => Some(gtt_total),
				MemoryKind::Dedicated => Some(vram_total),
				_ => None,
			};

			let numa_node = common::read_i32(&device_dir.join("numa_node"));
			let local_cpus = common::read_string_first_line(&device_dir.join("local_cpulist"))
				.map(|s| common::parse_cpulist(&s))
				.unwrap_or_default();
			let iommu_group = common::read_string(&device_dir.join("iommu_group/type"))
				.and_then(|_| {
					std::fs::read_link(device_dir.join("iommu_group"))
						.ok()
						.and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
				})
				.and_then(|s| s.parse::<u32>().ok());

			out.push(Accelerator {
				id: AcceleratorId { vendor: Vendor::Amd, drm_index, pci_addr },
				model,
				memory_kind,
				memory_total_bytes,
				numa_node,
				local_cpus,
				iommu_group,
				coverage: Coverage::Full,
				fabric_domain: None,
				partitioned: false,
				device_dir,
			});
		}
		Ok(out)
	}

	async fn snapshot(&self, accel: &Accelerator) -> anyhow::Result<Snapshot> {
		let dir = &accel.device_dir;
		let mut m = Vec::new();

		if let Some(v) = common::read_u64(&dir.join("mem_info_vram_total")) {
			m.push(Measurement {
				name: "accel.memory.vram.total",
				unit: "By",
				description: "Dedicated VRAM size reported by amdgpu (small on iGPUs).",
				value: v as f64,
				attrs: Vec::new(),
			});
		}
		if let Some(v) = common::read_u64(&dir.join("mem_info_vram_used")) {
			m.push(Measurement {
				name: "accel.memory.vram.used",
				unit: "By",
				description: "Used VRAM bytes.",
				value: v as f64,
				attrs: Vec::new(),
			});
		}
		if let Some(v) = common::read_u64(&dir.join("mem_info_gtt_total")) {
			m.push(Measurement {
				name: "accel.memory.gtt.total",
				unit: "By",
				description: "GTT (host aperture) size — this is the UMA carrier on iGPUs.",
				value: v as f64,
				attrs: Vec::new(),
			});
		}
		if let Some(v) = common::read_u64(&dir.join("mem_info_gtt_used")) {
			m.push(Measurement {
				name: "accel.memory.gtt.used",
				unit: "By",
				description: "Used GTT bytes.",
				value: v as f64,
				attrs: Vec::new(),
			});
		}
		if let Some(v) = common::read_u64(&dir.join("mem_info_visible_vram_total")) {
			m.push(Measurement {
				name: "accel.memory.visible_vram.total",
				unit: "By",
				description: "CPU-visible portion of VRAM (BAR1/Resizable-BAR aperture).",
				value: v as f64,
				attrs: Vec::new(),
			});
		}
		if let Some(v) = common::read_u64(&dir.join("gpu_busy_percent")) {
			m.push(Measurement {
				name: "accel.utilization",
				unit: "1",
				description: "Approximate GPU busy fraction (gpu_busy_percent / 100).",
				value: v as f64 / 100.0,
				attrs: Vec::new(),
			});
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
				m.push(Measurement {
					name: "accel.power.usage",
					unit: "W",
					description: "Average board power from hwmon power1_average.",
					value: p as f64 / 1_000_000.0,
					attrs: Vec::new(),
				});
			}
		}

		// Sensor health: even if we read nothing else, `device/vendor` returning
		// the right thing means amdgpu hasn't crashed out under us.
		let healthy = common::read_hex_u16(&dir.join("vendor")).map(|v| v == 0x1002).unwrap_or(false);
		m.push(Measurement {
			name: "accel.sensor.health",
			unit: "1",
			description: "1 = sensor reads succeeding, 0 = backend broken.",
			value: if healthy { 1.0 } else { 0.0 },
			attrs: Vec::new(),
		});

		Ok(Snapshot { measurements: m })
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
	let label_path = hw.join(format!("{prefix}_label"));
	common::read_string_first_line(&label_path).unwrap_or_else(|| prefix.to_string())
}
