// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Topology discovery + label-set construction. The topology graph is
//! consumed both by the K8s labeler (publishing per-node facts for
//! topology-aware schedulers) and by preflight checks scoped to
//! `NodeTopology`.

pub mod discover;
pub mod labels;

use serde::Serialize;

use crate::sensors::AcceleratorId;

#[derive(Debug, Clone, Default, Serialize)]
pub struct NodeTopology {
	pub region: Option<String>,
	pub zone: Option<String>,
	pub block: Option<String>,
	pub rack: Option<String>,
	pub fabric_domains: Vec<FabricDomain>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FabricDomain {
	/// Opaque, stable within a node. Identity-by-content: equal IDs iff
	/// equal member sets (sorted PCI addresses, hashed).
	pub id: String,
	pub kind: FabricKind,
	pub member_accelerators: Vec<AcceleratorId>,
	pub aggregate_bandwidth_gbps: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)] // NVLink variant ready for future feature-gated NVML backend.
pub enum FabricKind {
	/// AMD xGMI mesh (Instinct).
	XGmi,
	/// Nvidia NVLink — only discoverable via NVML; sysfs-only mode never
	/// emits this.
	NVLink,
	/// PCIe root complex / unknown — single-card fallback.
	Pcie,
}
