// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! A small set of real preflight checks that fit the sysfs-only design.
//! None of these link to vendor SDKs; all of them operate on values the
//! sensors already gathered, or on auxiliary sysfs reads scoped to the
//! accelerator at hand.

use async_trait::async_trait;

use crate::sensors::common;
use crate::sensors::{Coverage, Measurement, MemoryKind, Vendor};

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
			.map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("renderD")))
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
		Self { threshold_celsius: 95.0 }
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
				message: Some(format!("temperature {temp_c:.1} °C ≥ {} °C threshold", self.threshold_celsius)),
				measurements,
			}
		} else if temp_c >= warn_threshold {
			CheckOutcome {
				status: Status::Warn,
				message: Some(format!("temperature {temp_c:.1} °C is within 10 °C of threshold")),
				measurements,
			}
		} else {
			CheckOutcome { status: Status::Pass, message: None, measurements }
		}
	}
}

/// At least N bytes of allocatable accelerator memory must be free.
/// Configurable per-instance; defaults to 1 GiB. For UMA accelerators
/// we read GTT free; for dedicated, VRAM free.
pub struct MemoryFloor {
	pub min_free_bytes: u64,
}

impl Default for MemoryFloor {
	fn default() -> Self {
		Self { min_free_bytes: 1 << 30 } // 1 GiB
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
		let (total_path, used_path) = match a.memory_kind {
			MemoryKind::Unified => ("mem_info_gtt_total", "mem_info_gtt_used"),
			MemoryKind::Dedicated => ("mem_info_vram_total", "mem_info_vram_used"),
			_ => return CheckOutcome::skipped_with("memory kind doesn't expose sysfs accounting"),
		};
		let dir = &a.device_dir;
		let Some(total) = common::read_u64(&dir.join(total_path)) else {
			return CheckOutcome::skipped_with(format!("missing {total_path}"));
		};
		let used = common::read_u64(&dir.join(used_path)).unwrap_or(0);
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
			CheckOutcome { status: Status::Pass, message: None, measurements }
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
			CheckOutcome::warn(format!("driver={driver_name}, expected {}", expected_driver(a.id.vendor)))
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
