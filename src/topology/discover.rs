// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Vendor-agnostic node topology discovery. xGMI graphs come from sysfs
//! (`/sys/class/drm/cardN/device/xgmi_*`); NVLink is unreachable without
//! NVML and falls back to single-card domains.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::config::Args;
use crate::sensors::{Accelerator, AcceleratorId, Vendor};

use super::{FabricDomain, FabricKind, NodeTopology};

pub fn discover(args: &Args, accelerators: &mut [Accelerator]) -> NodeTopology {
	let mut topology = NodeTopology {
		region: std::env::var("ACCEL_READINESS_REGION").ok(),
		zone: std::env::var("ACCEL_READINESS_ZONE").ok(),
		block: args.block.clone(),
		rack: args.rack.clone(),
		fabric_domains: Vec::new(),
	};

	// Group accelerators into fabric domains. Per-vendor:
	//   AMD: walk xgmi_* sysfs to find connected components.
	//   Nvidia: sysfs-only ⇒ each card is its own single-member PCIe domain.
	//   Intel/Other: same single-member PCIe domain.
	let amd_groups = build_amd_xgmi_groups(accelerators);
	for (members, kind) in amd_groups {
		let id = stable_domain_id(&members);
		for m in &members {
			if let Some(a) = accelerators.iter_mut().find(|a| a.id == *m) {
				a.fabric_domain = Some(id.clone());
			}
		}
		topology.fabric_domains.push(FabricDomain {
			id,
			kind,
			member_accelerators: members,
			aggregate_bandwidth_gbps: None,
		});
	}

	// Single-card fallbacks for non-AMD or AMD with no xGMI peers.
	for accel in accelerators.iter_mut() {
		if accel.fabric_domain.is_some() {
			continue;
		}
		let id = stable_domain_id(std::slice::from_ref(&accel.id));
		accel.fabric_domain = Some(id.clone());
		topology.fabric_domains.push(FabricDomain {
			id,
			kind: FabricKind::Pcie,
			member_accelerators: vec![accel.id.clone()],
			aggregate_bandwidth_gbps: None,
		});
	}

	topology
}

fn build_amd_xgmi_groups(accels: &[Accelerator]) -> Vec<(Vec<AcceleratorId>, FabricKind)> {
	let amd: Vec<&Accelerator> = accels.iter().filter(|a| a.id.vendor == Vendor::Amd).collect();
	let mut adjacency: HashMap<AcceleratorId, BTreeSet<AcceleratorId>> = HashMap::new();
	for a in &amd {
		adjacency.entry(a.id.clone()).or_default();
		// xgmi_link_count tells us how many peer links this card has;
		// xgmi_peer_links lists peer PCI addresses (one per link).
		let count_path = a.device_dir.join("xgmi_link_count");
		if !count_path.exists() {
			continue;
		}
		let peers = read_xgmi_peers(&a.device_dir);
		for peer in peers {
			if let Some(peer_id) = amd.iter().find(|p| p.id.pci_addr == peer).map(|p| p.id.clone()) {
				adjacency.entry(a.id.clone()).or_default().insert(peer_id.clone());
				adjacency.entry(peer_id).or_default().insert(a.id.clone());
			}
		}
	}

	// Connected components by BFS.
	let mut visited: BTreeSet<AcceleratorId> = BTreeSet::new();
	let mut groups: Vec<(Vec<AcceleratorId>, FabricKind)> = Vec::new();
	for start in adjacency.keys().cloned().collect::<Vec<_>>() {
		if visited.contains(&start) {
			continue;
		}
		let neighbors = adjacency.get(&start).cloned().unwrap_or_default();
		// Lone AMD cards (no xGMI peers) handled by the single-card fallback.
		if neighbors.is_empty() {
			continue;
		}
		let mut queue = vec![start.clone()];
		let mut group: Vec<AcceleratorId> = Vec::new();
		while let Some(n) = queue.pop() {
			if !visited.insert(n.clone()) {
				continue;
			}
			group.push(n.clone());
			if let Some(next) = adjacency.get(&n) {
				for m in next {
					if !visited.contains(m) {
						queue.push(m.clone());
					}
				}
			}
		}
		group.sort_by(|a, b| a.pci_addr.cmp(&b.pci_addr));
		groups.push((group, FabricKind::XGmi));
	}
	groups
}

/// xGMI peer list lives in a few different sysfs files across kernel
/// versions. Try the well-known names; return an empty list if none work.
fn read_xgmi_peers(device_dir: &Path) -> Vec<String> {
	for name in &["xgmi_peer_links", "xgmi_phy_id", "xgmi_link_status"] {
		let p = device_dir.join(name);
		if let Ok(s) = std::fs::read_to_string(&p) {
			let peers: Vec<String> = s
				.split_ascii_whitespace()
				.filter(|tok| tok.contains(':'))
				.map(|tok| tok.to_string())
				.collect();
			if !peers.is_empty() {
				return peers;
			}
		}
	}
	Vec::new()
}

/// Build a short, stable domain ID from a set of accelerator IDs. Same
/// member set ⇒ same ID across reboots and across nodes (so two nodes
/// declaring the same fabric-domain ID actually mean the same fabric —
/// useful for cross-node intent like RDMA-fabric IDs that are config-fed,
/// not relevant for intra-node IDs which are always node-local).
pub fn stable_domain_id(members: &[AcceleratorId]) -> String {
	let mut sorted: Vec<String> = members.iter().map(|m| m.pci_addr.clone()).collect();
	sorted.sort();
	let mut hasher = DefaultHasher::new();
	for m in &sorted {
		m.hash(&mut hasher);
	}
	format!("fab-{:x}", hasher.finish() & 0xffff_ffff)
}
