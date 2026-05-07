// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Vendor-agnostic accelerator sensor abstraction. Backends read sysfs/proc
//! only — no vendor SDK linkage. Backends that can't fully populate a
//! snapshot return what they can and report reduced coverage via the
//! `coverage` field; they do NOT fabricate zeros.

pub mod amd;
pub mod common;
pub mod nvidia;

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
	Amd,
	Nvidia,
	Intel,
	Other,
}

impl Vendor {
	pub fn from_pci_id(vendor_hex: u16) -> Self {
		match vendor_hex {
			0x1002 => Vendor::Amd,
			0x10de => Vendor::Nvidia,
			0x8086 => Vendor::Intel,
			_ => Vendor::Other,
		}
	}

	pub fn slug(self) -> &'static str {
		match self {
			Vendor::Amd => "amd",
			Vendor::Nvidia => "nvidia",
			Vendor::Intel => "intel",
			Vendor::Other => "other",
		}
	}
}

impl fmt::Display for Vendor {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.slug())
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // Shared/Unknown variants are placeholders for vGPU/MIG futures.
pub enum MemoryKind {
	/// Dedicated VRAM — typical discrete GPU.
	Dedicated,
	/// Unified — accelerator uses host memory through GTT/IOMMU aperture
	/// (typical iGPU).
	Unified,
	/// Shared dedicated VRAM with multi-tenant carving (vGPU/MIG-aware
	/// stubs would land here).
	Shared,
	/// Coverage too thin to determine.
	Unknown,
}

impl MemoryKind {
	pub fn slug(self) -> &'static str {
		match self {
			MemoryKind::Dedicated => "dedicated",
			MemoryKind::Unified => "unified",
			MemoryKind::Shared => "shared",
			MemoryKind::Unknown => "unknown",
		}
	}
}

/// "How much detail can this sensor produce per-snapshot?"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)] // Failed variant ready for future wiring (sensor health drops to 0).
pub enum Coverage {
	/// Memory totals + utilization + temperature available.
	Full,
	/// Memory totals available, but live util/temp/power are not — the
	/// sysfs surface is too thin (Nvidia closed driver without NVML).
	IdentificationOnly,
	/// Sensor is broken; reads are failing.
	Failed,
}

impl Coverage {
	pub fn slug(self) -> &'static str {
		match self {
			Coverage::Full => "full",
			Coverage::IdentificationOnly => "identification-only",
			Coverage::Failed => "failed",
		}
	}
}

/// Stable, opaque accelerator identity within a node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct AcceleratorId {
	pub vendor: Vendor,
	/// `card0`, `card1`, … (DRM minor index).
	pub drm_index: u32,
	/// `0000:01:00.0` style PCI domain:bus:dev.func.
	pub pci_addr: String,
}

impl fmt::Display for AcceleratorId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "card{} [{}] {}", self.drm_index, self.pci_addr, self.vendor)
	}
}

/// What `enumerate()` produces — static facts about an accelerator that
/// don't change tick-to-tick.
#[derive(Debug, Clone, Serialize)]
pub struct Accelerator {
	pub id: AcceleratorId,
	pub model: String,
	pub memory_kind: MemoryKind,
	pub memory_total_bytes: Option<u64>,
	pub numa_node: Option<i32>,
	pub local_cpus: Vec<usize>,
	pub iommu_group: Option<u32>,
	pub coverage: Coverage,
	/// Filled in by topology pass after enumeration.
	pub fabric_domain: Option<String>,
	/// Best-effort: if multiple DRM cards share a parent PCI function,
	/// they're partitions of the same physical accelerator (MIG / SR-IOV).
	pub partitioned: bool,
	/// Sysfs device dir — useful for snapshots without re-walking.
	#[serde(skip)]
	pub device_dir: PathBuf,
}

/// Per-tick snapshot — what changes during runtime.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
	pub measurements: Vec<Measurement>,
}

#[derive(Debug, Clone)]
pub struct Measurement {
	pub name: &'static str,
	pub unit: &'static str,
	pub description: &'static str,
	pub value: f64,
	/// Extra attributes beyond the per-accelerator ones the caller
	/// always adds.
	pub attrs: Vec<(&'static str, String)>,
}

#[async_trait]
pub trait AcceleratorSensor: Send + Sync {
	fn vendor(&self) -> Vendor;

	/// Cheap — called once at startup per backend.
	async fn enumerate(&self) -> anyhow::Result<Vec<Accelerator>>;

	/// Called on every live tick. Bounded latency required.
	async fn snapshot(&self, accel: &Accelerator) -> anyhow::Result<Snapshot>;
}

/// Walk `/sys/class/drm/card*` and dispatch each card to the right vendor
/// backend. Cards behind unrecognised PCI vendors are skipped (logged).
pub async fn enumerate_all() -> anyhow::Result<(Vec<Accelerator>, Vec<Box<dyn AcceleratorSensor>>)> {
	let mut all = Vec::new();
	let backends: Vec<Box<dyn AcceleratorSensor>> =
		vec![Box::new(amd::AmdSysfsSensor), Box::new(nvidia::NvidiaSysfsSensor)];

	for backend in &backends {
		match backend.enumerate().await {
			Ok(mut accels) => {
				tracing::info!(vendor = %backend.vendor(), found = accels.len(), "enumerated");
				all.append(&mut accels);
			}
			Err(e) => {
				tracing::warn!(vendor = %backend.vendor(), error = %e, "enumeration failed; continuing");
			}
		}
	}

	// Detect partitions: multiple DRM cards sharing the same parent PCI function.
	common::mark_partitions(&mut all);

	Ok((all, backends))
}
