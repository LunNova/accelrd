// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! A small set of real preflight checks that fit the sysfs-only design.
//! None of these link to vendor SDKs; all of them operate on values the
//! sensors already gathered, or on auxiliary sysfs reads scoped to the
//! accelerator at hand.

use async_trait::async_trait;

use crate::sensors::common;
use crate::sensors::host;
use crate::sensors::{Coverage, Measurement, Vendor};

use super::{CheckContext, CheckOutcome, CheckScope, PreflightCheck, Status};

/// Macro helper: every `SingleAccelerator`-scoped check starts with the
/// same "no accelerator in context, return Skipped" early return. Keeping
/// it as a small macro avoids importing `?`-tries for `Option` from a
/// trait return that isn't `Option`.
macro_rules! accel_or_skip {
	($ctx:expr) => {
		match $ctx.accelerator {
			Some(a) => a,
			None => return CheckOutcome::skipped(),
		}
	};
}

/// Fail when our own sensor reads are failing — the rest of the picture
/// is meaningless if we can't even read sysfs.
pub struct SensorReadable;

#[async_trait]
impl PreflightCheck for SensorReadable {
	fn name(&self) -> &'static str {
		"sensor.readable"
	}
	fn scope(&self) -> CheckScope {
		CheckScope::SingleAccelerator
	}
	async fn run(&self, ctx: &CheckContext<'_>) -> CheckOutcome {
		let a = accel_or_skip!(ctx);
		match a.coverage {
			Coverage::Failed => CheckOutcome::fail(format!("sensor backend reports Failed coverage for {}", a.id)),
			Coverage::IdentificationOnly | Coverage::Full => CheckOutcome::pass(),
		}
	}
}

/// DRM render node accessibility. If `/dev/dri/renderD<128+drm_index>`
/// is absent or unreadable, the accelerator is effectively offline for
/// userspace consumers.
pub struct DrmRenderNodePresent;

#[async_trait]
impl PreflightCheck for DrmRenderNodePresent {
	fn name(&self) -> &'static str {
		"drm.render_node_present"
	}
	fn scope(&self) -> CheckScope {
		CheckScope::SingleAccelerator
	}
	async fn run(&self, ctx: &CheckContext<'_>) -> CheckOutcome {
		let a = accel_or_skip!(ctx);
		// Render nodes start at 128 by convention; resolve the actual
		// minor by reading /sys/class/drm/cardN/device/drm/renderD*.
		let drm_dir = a.device_dir.join("drm");
		let exists = std::fs::read_dir(&drm_dir)
			.map(|d| {
				d.flatten()
					.any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
			})
			.unwrap_or(false);
		if exists {
			CheckOutcome::pass()
		} else {
			CheckOutcome::fail(format!("no renderD node under {}", drm_dir.display()))
		}
	}
}

/// Junction temperature must be safely below the per-vendor throttle
/// threshold. Skipped when the sensor doesn't expose temperature
/// (Nvidia closed-driver). The threshold defaults to 95 °C — most
/// vendor parts throttle around 100-110 °C, so 95 leaves headroom.
pub struct TemperatureBelowThrottle {
	pub threshold_celsius: f64,
}

impl Default for TemperatureBelowThrottle {
	fn default() -> Self {
		Self {
			threshold_celsius: 95.0,
		}
	}
}

#[async_trait]
impl PreflightCheck for TemperatureBelowThrottle {
	fn name(&self) -> &'static str {
		"temperature.below_throttle"
	}
	fn scope(&self) -> CheckScope {
		CheckScope::SingleAccelerator
	}
	async fn run(&self, ctx: &CheckContext<'_>) -> CheckOutcome {
		let a = accel_or_skip!(ctx);
		let Some(temp_c) = common::hwmon_temperature_celsius(&a.device_dir) else {
			return CheckOutcome::skipped_with("no hwmon temp1_input on this accelerator");
		};
		let measurements = vec![Measurement {
			name: "accel.preflight.temperature",
			unit: "Cel",
			description: "Temperature observed by the throttle preflight check.",
			value: temp_c,
			attrs: Vec::new(),
		}];
		let warn_threshold = self.threshold_celsius - 10.0;
		if temp_c >= self.threshold_celsius {
			CheckOutcome {
				status: Status::Fail,
				message: Some(format!(
					"temperature {temp_c:.1} °C ≥ {} °C threshold",
					self.threshold_celsius
				)),
				measurements,
			}
		} else if temp_c >= warn_threshold {
			CheckOutcome {
				status: Status::Warn,
				message: Some(format!("temperature {temp_c:.1} °C is within 10 °C of threshold")),
				measurements,
			}
		} else {
			CheckOutcome {
				status: Status::Pass,
				message: None,
				measurements,
			}
		}
	}
}

/// At least N bytes of allocatable accelerator memory must be free.
/// Configurable per-instance; defaults to 1 GiB. Reads the amdgpu
/// `mem_info_vram_*` accounting that's always present on AMD cards
/// (HBM on Instinct, GDDR/HBM on Radeon, the GPU-addressable pool on
/// APUs); skips other vendors where we can't observe used memory
/// without a vendor SDK.
pub struct MemoryFloor {
	pub min_free_bytes: u64,
}

impl Default for MemoryFloor {
	fn default() -> Self {
		Self {
			min_free_bytes: 1 << 30,
		} // 1 GiB
	}
}

#[async_trait]
impl PreflightCheck for MemoryFloor {
	fn name(&self) -> &'static str {
		"memory.floor"
	}
	fn scope(&self) -> CheckScope {
		CheckScope::SingleAccelerator
	}
	async fn run(&self, ctx: &CheckContext<'_>) -> CheckOutcome {
		let a = accel_or_skip!(ctx);
		// Skip for sensors that don't expose used/total — would need NVML.
		if matches!(a.coverage, Coverage::IdentificationOnly) {
			return CheckOutcome::skipped_with("identification-only sensor cannot report free memory");
		}
		let dir = &a.device_dir;
		let Some(total) = common::read_u64(&dir.join("mem_info_vram_total")) else {
			return CheckOutcome::skipped_with("missing mem_info_vram_total");
		};
		let used = common::read_u64(&dir.join("mem_info_vram_used")).unwrap_or(0);
		let free = total.saturating_sub(used);
		let measurements = vec![Measurement {
			name: "accel.preflight.memory.free",
			unit: "By",
			description: "Free memory observed by the floor preflight check.",
			value: free as f64,
			attrs: Vec::new(),
		}];
		if free < self.min_free_bytes {
			CheckOutcome {
				status: Status::Fail,
				message: Some(format!("free {free} < required {} bytes", self.min_free_bytes)),
				measurements,
			}
		} else {
			CheckOutcome {
				status: Status::Pass,
				message: None,
				measurements,
			}
		}
	}
}

/// Driver presence: a known kernel driver string for the vendor must be
/// readable. Catches "kernel module loaded but broken" cases like
/// nvidia.ko being unloaded under a running daemon.
pub struct DriverLoaded;

#[async_trait]
impl PreflightCheck for DriverLoaded {
	fn name(&self) -> &'static str {
		"driver.loaded"
	}
	fn scope(&self) -> CheckScope {
		CheckScope::SingleAccelerator
	}
	async fn run(&self, ctx: &CheckContext<'_>) -> CheckOutcome {
		let a = accel_or_skip!(ctx);
		let Ok(target) = std::fs::read_link(a.device_dir.join("driver")) else {
			return CheckOutcome::fail("device/driver symlink missing — kernel module not bound?");
		};
		let driver_name = target.file_name().and_then(|s| s.to_str()).unwrap_or("?");
		if accepts_driver(a.id.vendor, driver_name) {
			CheckOutcome::pass()
		} else {
			CheckOutcome::warn(format!(
				"driver={driver_name}, expected {}",
				expected_driver(a.id.vendor)
			))
		}
	}
}

/// Host RAM has at least N bytes available before launching a GPU job.
/// Uses MemAvailable, not MemFree — MemAvailable accounts for reclaimable
/// page cache so we don't false-fail busy-but-fine hosts. Default 24 GiB
/// matches the threshold below which CUDA/HIP context init + medium-model
/// weight loading start failing on real workloads.
pub struct HostMemoryAvailable {
	pub min_available_bytes: u64,
}

impl Default for HostMemoryAvailable {
	fn default() -> Self {
		Self {
			min_available_bytes: 24u64 << 30,
		}
	}
}

#[async_trait]
impl PreflightCheck for HostMemoryAvailable {
	fn name(&self) -> &'static str {
		"host.memory.available"
	}
	fn scope(&self) -> CheckScope {
		CheckScope::NodeLocal
	}
	async fn run(&self, _ctx: &CheckContext<'_>) -> CheckOutcome {
		let snap = host::snapshot();
		let Some(avail) = snap
			.iter()
			.find(|m| m.name == "host.memory.available_bytes")
			.map(|m| m.value as u64)
		else {
			return CheckOutcome::skipped_with("MemAvailable unreadable from /proc/meminfo");
		};
		let measurements = vec![Measurement {
			name: "host.preflight.memory.available",
			unit: "By",
			description: "MemAvailable observed by the host-memory preflight check.",
			value: avail as f64,
			attrs: Vec::new(),
		}];
		if avail < self.min_available_bytes {
			CheckOutcome {
				status: Status::Fail,
				message: Some(format!(
					"MemAvailable {} < required {} bytes",
					avail, self.min_available_bytes
				)),
				measurements,
			}
		} else {
			CheckOutcome {
				status: Status::Pass,
				message: None,
				measurements,
			}
		}
	}
}

/// At least one real (non-virtual) filesystem has N bytes free. We pick
/// the largest mount because workload state (image pulls, model
/// checkpoints, ephemeral volumes) lands wherever the container runtime
/// happens to put it; the largest mount is almost always that target.
/// Default 200 GiB matches the user's heuristic for "we have space for
/// a model + image + checkpoints without falling over."
pub struct HostDiskFree {
	pub min_free_bytes: u64,
}

impl Default for HostDiskFree {
	fn default() -> Self {
		Self {
			min_free_bytes: 200u64 << 30,
		}
	}
}

#[async_trait]
impl PreflightCheck for HostDiskFree {
	fn name(&self) -> &'static str {
		"host.disk.free"
	}
	fn scope(&self) -> CheckScope {
		CheckScope::NodeLocal
	}
	async fn run(&self, _ctx: &CheckContext<'_>) -> CheckOutcome {
		let snap = host::snapshot();
		let mut best: Option<(String, u64)> = None;
		for m in snap.iter().filter(|m| m.name == "host.disk.free_bytes") {
			let mount = m
				.attrs
				.iter()
				.find(|(k, _)| *k == "mount")
				.map(|(_, v)| v.clone())
				.unwrap_or_default();
			let bytes = m.value as u64;
			if best.as_ref().map(|(_, b)| bytes > *b).unwrap_or(true) {
				best = Some((mount, bytes));
			}
		}
		let Some((mount, free)) = best else {
			return CheckOutcome::skipped_with("no real-filesystem mounts found in /proc/self/mountinfo");
		};
		let measurements = vec![Measurement {
			name: "host.preflight.disk.free",
			unit: "By",
			description: "Free bytes on the largest real mount.",
			value: free as f64,
			attrs: vec![("mount", mount.clone())],
		}];
		if free < self.min_free_bytes {
			CheckOutcome {
				status: Status::Fail,
				message: Some(format!(
					"largest mount {mount} has {free} free < required {} bytes",
					self.min_free_bytes
				)),
				measurements,
			}
		} else {
			CheckOutcome {
				status: Status::Pass,
				message: None,
				measurements,
			}
		}
	}
}

fn expected_driver(vendor: Vendor) -> &'static str {
	match vendor {
		Vendor::Amd => "amdgpu",
		Vendor::Nvidia => "nvidia",
		// Both i915 (legacy) and xe (new) are acceptable Intel paths.
		Vendor::Intel => "i915 or xe",
		Vendor::Other => "?",
	}
}

fn accepts_driver(vendor: Vendor, name: &str) -> bool {
	match vendor {
		Vendor::Amd => name == "amdgpu",
		Vendor::Nvidia => name == "nvidia",
		Vendor::Intel => name == "i915" || name == "xe",
		Vendor::Other => false,
	}
}
