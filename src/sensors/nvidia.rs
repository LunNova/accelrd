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

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::common;
use super::{Accelerator, AcceleratorId, AcceleratorSensor, Coverage, Measurement, MemoryKind, Snapshot, Vendor};

pub struct NvidiaSysfsSensor;

#[async_trait]
impl AcceleratorSensor for NvidiaSysfsSensor {
	fn vendor(&self) -> Vendor {
		Vendor::Nvidia
	}

	async fn enumerate(&self) -> anyhow::Result<Vec<Accelerator>> {
		let mut out = Vec::new();
		let proc_info = read_proc_driver_nvidia();

		for (drm_index, device_dir) in common::drm_cards() {
			let vendor_id = common::read_hex_u16(&device_dir.join("vendor")).unwrap_or(0);
			if Vendor::from_pci_id(vendor_id) != Vendor::Nvidia {
				continue;
			}
			let device_id = common::read_hex_u16(&device_dir.join("device")).unwrap_or(0);
			let pci_addr = common::pci_addr_from_device_dir(&device_dir).unwrap_or_default();

			// Look up /proc/driver/nvidia/gpus/<bus>/information for the
			// canonical model name. The bus key in /proc is `<dom>:<bus>:<dev>.<fn>`.
			let proc_entry = proc_info.gpus.iter().find(|g| g.bus_id.eq_ignore_ascii_case(&pci_addr));
			let model = proc_entry
				.and_then(|g| g.model.clone())
				.unwrap_or_else(|| format!("Nvidia device {device_id:#06x}"));

			// Memory total from PCI BAR1: NVIDIA discrete cards expose VRAM as
			// BAR1 (64-bit prefetchable). On a 4090 this is 32 GiB.
			let bars = common::read_pci_bars(&device_dir);
			let bar1 = bars.get(1).copied().unwrap_or(0);
			let memory_total_bytes = if bar1 > (4 * 1024 * 1024) { Some(bar1) } else { None };

			let numa_node = common::read_i32(&device_dir.join("numa_node"));
			let local_cpus = common::read_string_first_line(&device_dir.join("local_cpulist"))
				.map(|s| common::parse_cpulist(&s))
				.unwrap_or_default();

			out.push(Accelerator {
				id: AcceleratorId { vendor: Vendor::Nvidia, drm_index, pci_addr },
				model,
				memory_kind: if memory_total_bytes.is_some() { MemoryKind::Dedicated } else { MemoryKind::Unknown },
				memory_total_bytes,
				numa_node,
				local_cpus,
				iommu_group: None,
				coverage: Coverage::IdentificationOnly,
				fabric_domain: None,
				partitioned: false,
				device_dir,
			});
		}

		Ok(out)
	}

	async fn snapshot(&self, accel: &Accelerator) -> anyhow::Result<Snapshot> {
		let mut m = Vec::new();

		if let Some(total) = accel.memory_total_bytes {
			m.push(Measurement {
				name: "accel.memory.dedicated.total",
				unit: "By",
				description: "Dedicated VRAM total (from PCI BAR1; sysfs-only path).",
				value: total as f64,
				attrs: Vec::new(),
			});
		}

		// Open-driver hwmon (best effort). nvidia-open exposes some entries;
		// the closed driver does not.
		for hw in common::hwmon_dirs(&accel.device_dir) {
			if let Some(t) = common::read_u64(&hw.join("temp1_input")) {
				m.push(Measurement {
					name: "accel.temperature",
					unit: "Cel",
					description: "Edge temperature from open-driver hwmon.",
					value: t as f64 / 1000.0,
					attrs: Vec::new(),
				});
			}
		}

		// Sensor health — non-zero as long as basic sysfs reads work.
		let healthy = common::read_hex_u16(&accel.device_dir.join("vendor")).map(|v| v == 0x10de).unwrap_or(false);
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

#[derive(Debug, Default, Clone)]
struct ProcNvidia {
	driver_version: Option<String>,
	gpus: Vec<ProcGpu>,
}

#[derive(Debug, Clone)]
struct ProcGpu {
	bus_id: String,    // formatted like 0000:01:00.0
	model: Option<String>,
}

/// Parse `/proc/driver/nvidia/version` and `/proc/driver/nvidia/gpus/*/information`.
fn read_proc_driver_nvidia() -> ProcNvidia {
	let mut out = ProcNvidia::default();
	out.driver_version = fs::read_to_string("/proc/driver/nvidia/version")
		.ok()
		.and_then(|s| s.lines().next().map(|l| l.trim().to_string()));

	let gpus_dir = Path::new("/proc/driver/nvidia/gpus");
	let Ok(entries) = fs::read_dir(gpus_dir) else { return out };
	for entry in entries.flatten() {
		let bus_dir: PathBuf = entry.path();
		let Some(bus_id) = bus_dir.file_name().and_then(|s| s.to_str()).map(|s| s.to_string()) else {
			continue;
		};
		let info_path = bus_dir.join("information");
		let Ok(text) = fs::read_to_string(&info_path) else { continue };
		let mut model = None;
		for line in text.lines() {
			if let Some((k, v)) = line.split_once(':') {
				let k = k.trim();
				let v = v.trim();
				if k == "Model" {
					model = Some(v.to_string());
				}
			}
		}
		out.gpus.push(ProcGpu { bus_id, model });
	}
	out
}
