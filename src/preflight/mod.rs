// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Preflight check trait + registry. Real checks (rccl loopback, ECC,
//! firmware version, fabric integrity) are out of scope this lift; the
//! shape is here so they can be plugged in without redesign.

pub mod health;
pub mod placeholder;

use async_trait::async_trait;

use crate::sensors::{Accelerator, Measurement};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // NodeLocal/NodeTopology/Cluster scopes ready for real checks.
pub enum CheckScope {
	SingleAccelerator,
	NodeLocal,
	NodeTopology,
	Cluster,
}

impl CheckScope {
	pub fn slug(self) -> &'static str {
		match self {
			CheckScope::SingleAccelerator => "single-accelerator",
			CheckScope::NodeLocal => "node-local",
			CheckScope::NodeTopology => "node-topology",
			CheckScope::Cluster => "cluster",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
	Pass,
	Warn,
	Fail,
	/// Coverage on this accelerator can't answer the check (e.g. a
	/// temperature-throttle check on a sysfs-only Nvidia card). Skipped
	/// does NOT drag node readiness down — it's recorded so operators
	/// see what's uncovered.
	Skipped,
}

impl Status {
	pub fn slug(self) -> &'static str {
		match self {
			Status::Pass => "pass",
			Status::Warn => "warn",
			Status::Fail => "fail",
			Status::Skipped => "skipped",
		}
	}
}

pub struct CheckOutcome {
	pub status: Status,
	pub message: Option<String>,
	pub measurements: Vec<Measurement>,
}

impl CheckOutcome {
	/// Skipped outcome with no message — the common "no accelerator in
	/// context" early return for `SingleAccelerator`-scoped checks.
	pub fn skipped() -> Self {
		Self {
			status: Status::Skipped,
			message: None,
			measurements: Vec::new(),
		}
	}

	/// Skipped with an explanatory message.
	pub fn skipped_with(msg: impl Into<String>) -> Self {
		Self {
			status: Status::Skipped,
			message: Some(msg.into()),
			measurements: Vec::new(),
		}
	}

	/// Pass with no measurements.
	pub fn pass() -> Self {
		Self {
			status: Status::Pass,
			message: None,
			measurements: Vec::new(),
		}
	}

	/// Fail with an explanatory message.
	pub fn fail(msg: impl Into<String>) -> Self {
		Self {
			status: Status::Fail,
			message: Some(msg.into()),
			measurements: Vec::new(),
		}
	}

	/// Warn with an explanatory message.
	pub fn warn(msg: impl Into<String>) -> Self {
		Self {
			status: Status::Warn,
			message: Some(msg.into()),
			measurements: Vec::new(),
		}
	}
}

pub struct CheckContext<'a> {
	pub accelerator: Option<&'a Accelerator>,
}

#[async_trait]
pub trait PreflightCheck: Send + Sync {
	fn name(&self) -> &'static str;
	fn scope(&self) -> CheckScope;
	fn applies_to(&self, _accel: &Accelerator) -> bool {
		true
	}
	async fn run(&self, ctx: &CheckContext<'_>) -> CheckOutcome;
}

/// Default check registry. Real, sysfs-friendly checks plus the
/// always-Pass placeholder so the trait shape stays exercised even when
/// every other check is Skipped.
pub fn default_registry() -> Vec<Box<dyn PreflightCheck>> {
	vec![
		Box::new(health::SensorReadable),
		Box::new(health::DrmRenderNodePresent),
		Box::new(health::DriverLoaded),
		Box::new(health::TemperatureBelowThrottle::default()),
		Box::new(health::MemoryFloor::default()),
		Box::new(health::HostMemoryAvailable::default()),
		Box::new(health::HostDiskFree::default()),
		Box::new(placeholder::AlwaysPass),
	]
}

/// Aggregate per-(check, accel) outcomes into a single node-level verdict
/// for a readiness class (inference / training). Fail and Warn both
/// degrade today; when stricter aggregation is needed, Fail will graduate
/// to `NotReady`.
pub fn aggregate(outcomes: &[Status]) -> NodeReadiness {
	if outcomes.iter().any(|s| matches!(s, Status::Fail | Status::Warn)) {
		NodeReadiness::Degraded
	} else if outcomes.contains(&Status::Pass) {
		NodeReadiness::Ready
	} else {
		NodeReadiness::Unknown
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // NotReady variant lands when real Fail-aggregating checks exist.
pub enum NodeReadiness {
	Ready,
	Degraded,
	NotReady,
	Unknown,
}

impl NodeReadiness {
	pub fn label_value(self) -> &'static str {
		match self {
			NodeReadiness::Ready => "true",
			NodeReadiness::Degraded => "degraded",
			NodeReadiness::NotReady => "false",
			NodeReadiness::Unknown => "unknown",
		}
	}
}
