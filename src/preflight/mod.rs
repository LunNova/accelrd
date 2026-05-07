// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Preflight check trait + registry. Real checks (rccl loopback, ECC,
//! firmware version, fabric integrity) are out of scope this lift; the
//! shape is here so they can be plugged in without redesign.

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
#[allow(dead_code)] // Warn/Fail constructed by real preflight checks (future).
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
	#[allow(dead_code)] // Real checks emit per-check measurements.
	pub measurements: Vec<Measurement>,
}

pub struct CheckContext<'a> {
	#[allow(dead_code)] // Used by real checks; placeholder ignores it.
	pub accelerator: Option<&'a Accelerator>,
}

#[async_trait]
pub trait PreflightCheck: Send + Sync {
	fn name(&self) -> &'static str;
	fn scope(&self) -> CheckScope;
	#[allow(unused_variables)]
	fn applies_to(&self, accel: &Accelerator) -> bool {
		true
	}
	async fn run(&self, ctx: &CheckContext<'_>) -> CheckOutcome;
}

/// Default registry — currently just the placeholder.
pub fn default_registry() -> Vec<Box<dyn PreflightCheck>> {
	vec![Box::new(placeholder::AlwaysPass)]
}

/// Aggregate per-(check, accel) outcomes into a single node-level verdict
/// for a readiness class (inference / training).
pub fn aggregate(outcomes: &[Status]) -> NodeReadiness {
	let mut any_fail = false;
	let mut any_warn = false;
	let mut any_pass = false;
	for s in outcomes {
		match s {
			Status::Fail => any_fail = true,
			Status::Warn => any_warn = true,
			Status::Pass => any_pass = true,
			Status::Skipped => {}
		}
	}
	if any_fail {
		NodeReadiness::Degraded
	} else if any_warn {
		NodeReadiness::Degraded
	} else if any_pass {
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
