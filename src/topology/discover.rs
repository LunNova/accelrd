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
	for members in build_amd_xgmi_groups(accelerators) {
		let id = stable_domain_id(&members);
		assign_fabric(accelerators, &members, &id);
		topology.fabric_domains.push(FabricDomain {
			id,
			kind: FabricKind::XGmi,
			member_accelerators: members,
			aggregate_bandwidth_gbps: None,
		});
	}

	// Single-card fallback for accelerators not yet placed in a fabric.
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

/// Build xGMI connected components from AMD accelerator sysfs. Lone AMD
/// cards (no peer links) are excluded — the single-card fallback handles
/// them — so this only emits multi-member groups.
fn build_amd_xgmi_groups(accels: &[Accelerator]) -> Vec<Vec<AcceleratorId>> {
	let amd: Vec<&Accelerator> = accels.iter().filter(|a| a.id.vendor == Vendor::Amd).collect();
	let mut adjacency: HashMap<&AcceleratorId, BTreeSet<&AcceleratorId>> = HashMap::new();

	for a in &amd {
		adjacency.entry(&a.id).or_default();
		for peer_addr in read_xgmi_peers(&a.device_dir) {
			let Some(peer) = amd.iter().find(|p| p.id.pci_addr == peer_addr) else { continue };
			adjacency.entry(&a.id).or_default().insert(&peer.id);
			adjacency.entry(&peer.id).or_default().insert(&a.id);
		}
	}

	// Connected components by BFS over the adjacency map.
	let mut visited: BTreeSet<&AcceleratorId> = BTreeSet::new();
	let mut groups: Vec<Vec<AcceleratorId>> = Vec::new();
	for &start in adjacency.keys() {
		if visited.contains(start) || adjacency[start].is_empty() {
			continue;
		}
		let mut group: Vec<AcceleratorId> = Vec::new();
		let mut queue = vec![start];
		while let Some(n) = queue.pop() {
			if !visited.insert(n) {
				continue;
			}
			group.push(n.clone());
			if let Some(neighbors) = adjacency.get(n) {
				queue.extend(neighbors.iter().filter(|m| !visited.contains(*m)));
			}
		}
		group.sort_by(|a, b| a.pci_addr.cmp(&b.pci_addr));
		groups.push(group);
	}
	groups
}

fn assign_fabric(accelerators: &mut [Accelerator], members: &[AcceleratorId], id: &str) {
	for m in members {
		if let Some(a) = accelerators.iter_mut().find(|a| a.id == *m) {
			a.fabric_domain = Some(id.to_string());
		}
	}
}

/// xGMI peer list lives in a few different sysfs files across kernel
/// versions. Try the well-known names; return an empty list if none work.
fn read_xgmi_peers(device_dir: &Path) -> Vec<String> {
	for name in ["xgmi_peer_links", "xgmi_phy_id", "xgmi_link_status"] {
		let Ok(s) = std::fs::read_to_string(device_dir.join(name)) else { continue };
		let peers: Vec<String> =
			s.split_ascii_whitespace().filter(|t| t.contains(':')).map(str::to_string).collect();
		if !peers.is_empty() {
			return peers;
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
	let mut sorted: Vec<&str> = members.iter().map(|m| m.pci_addr.as_str()).collect();
	sorted.sort();
	let mut hasher = DefaultHasher::new();
	for m in &sorted {
		m.hash(&mut hasher);
	}
	format!("fab-{:x}", hasher.finish() & 0xffff_ffff)
}
