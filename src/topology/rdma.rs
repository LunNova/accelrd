// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! RDMA presence + port-state discovery from `/sys/class/infiniband`.
//! Used by:
//!  - the daemon's labeler, to publish `accel-net.lunnova.dev/rdma=present`
//!    + a JSON inventory annotation on each node.
//!  - the prober's pair eligibility, to skip nodes that share a rack with
//!    RDMA hosts but can't actually run a probe themselves.
//!
//! Sysfs layout (Mellanox example):
//!   /sys/class/infiniband/mlx5_0/ports/1/state         "4: ACTIVE"
//!   /sys/class/infiniband/mlx5_0/ports/1/phys_state    "5: LinkUp"
//!   /sys/class/infiniband/mlx5_0/ports/1/link_layer    "Ethernet" (RoCE) | "InfiniBand"
//!   /sys/class/infiniband/mlx5_0/ports/1/rate          "200 Gb/sec (4X NDR)"

use std::fs;
use std::path::Path;

use serde::Serialize;

const SYSFS_INFINIBAND: &str = "/sys/class/infiniband";

#[derive(Debug, Clone, Default, Serialize)]
pub struct RdmaInventory {
	pub devices: Vec<RdmaDevice>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RdmaDevice {
	pub name: String,
	pub ports: Vec<RdmaPort>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RdmaPort {
	pub index: u32,
	pub state: String,
	pub phys_state: String,
	pub link_layer: String,
	pub rate_gbps: Option<u32>,
}

impl RdmaInventory {
	pub fn scan() -> Self {
		let Ok(entries) = fs::read_dir(SYSFS_INFINIBAND) else {
			return Self::default();
		};
		let mut devices: Vec<RdmaDevice> = entries
			.flatten()
			.filter_map(|e| {
				let name = e.file_name().into_string().ok()?;
				let ports = scan_ports(&e.path());
				Some(RdmaDevice { name, ports })
			})
			.collect();
		devices.sort_by(|a, b| a.name.cmp(&b.name));
		Self { devices }
	}

	pub fn is_empty(&self) -> bool {
		self.devices.is_empty()
	}

	/// Probe-eligibility predicate. RoCE = RDMA-over-Ethernet; native
	/// InfiniBand fabrics aren't reachable from our prober's pod-network
	/// pairing setup, so we require at least one ACTIVE Ethernet port.
	pub fn any_active_roce(&self) -> bool {
		self.devices.iter().any(|d| {
			d.ports
				.iter()
				.any(|p| p.state.eq_ignore_ascii_case("ACTIVE") && p.link_layer.eq_ignore_ascii_case("Ethernet"))
		})
	}
}

fn scan_ports(dev_dir: &Path) -> Vec<RdmaPort> {
	let ports_dir = dev_dir.join("ports");
	let Ok(entries) = fs::read_dir(&ports_dir) else {
		return Vec::new();
	};
	let mut out: Vec<RdmaPort> = entries
		.flatten()
		.filter_map(|e| {
			let idx: u32 = e.file_name().to_str()?.parse().ok()?;
			let p = e.path();
			Some(RdmaPort {
				index: idx,
				state: parse_enum(read_trim(&p.join("state"))).unwrap_or_default(),
				phys_state: parse_enum(read_trim(&p.join("phys_state"))).unwrap_or_default(),
				link_layer: read_trim(&p.join("link_layer")).unwrap_or_default(),
				rate_gbps: parse_rate(read_trim(&p.join("rate"))),
			})
		})
		.collect();
	out.sort_by_key(|p| p.index);
	out
}

fn read_trim(p: &Path) -> Option<String> {
	fs::read_to_string(p)
		.ok()
		.map(|s| s.trim().to_string())
		.filter(|s| !s.is_empty())
}

/// sysfs state files look like `4: ACTIVE`; the mnemonic is the last
/// whitespace-separated token. Returns the mnemonic alone (no `N:` prefix).
fn parse_enum(s: Option<String>) -> Option<String> {
	s.and_then(|v| v.split_whitespace().last().map(str::to_string))
}

/// sysfs `rate` looks like `200 Gb/sec (4X NDR)`; we keep only the leading integer.
fn parse_rate(s: Option<String>) -> Option<u32> {
	s.and_then(|v| v.split_whitespace().next().and_then(|n| n.parse().ok()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_state_mnemonic() {
		assert_eq!(parse_enum(Some("4: ACTIVE".into())).as_deref(), Some("ACTIVE"));
		assert_eq!(parse_enum(Some("5: LinkUp".into())).as_deref(), Some("LinkUp"));
		assert_eq!(parse_enum(Some("DOWN".into())).as_deref(), Some("DOWN"));
		assert!(parse_enum(None).is_none());
	}

	#[test]
	fn parses_rate_gbps() {
		assert_eq!(parse_rate(Some("200 Gb/sec (4X NDR)".into())), Some(200));
		assert_eq!(parse_rate(Some("100 Gb/sec".into())), Some(100));
		assert_eq!(parse_rate(Some("garbage".into())), None);
		assert_eq!(parse_rate(None), None);
	}

	#[test]
	fn any_active_roce_predicate() {
		let inv = RdmaInventory {
			devices: vec![RdmaDevice {
				name: "mlx5_0".into(),
				ports: vec![RdmaPort {
					index: 1,
					state: "ACTIVE".into(),
					phys_state: "LinkUp".into(),
					link_layer: "Ethernet".into(),
					rate_gbps: Some(200),
				}],
			}],
		};
		assert!(inv.any_active_roce());

		let inv_ib = RdmaInventory {
			devices: vec![RdmaDevice {
				name: "mlx5_0".into(),
				ports: vec![RdmaPort {
					index: 1,
					state: "ACTIVE".into(),
					phys_state: "LinkUp".into(),
					link_layer: "InfiniBand".into(),
					rate_gbps: Some(200),
				}],
			}],
		};
		assert!(!inv_ib.any_active_roce(), "native IB shouldn't count as RoCE");

		let inv_down = RdmaInventory {
			devices: vec![RdmaDevice {
				name: "mlx5_0".into(),
				ports: vec![RdmaPort {
					index: 1,
					state: "DOWN".into(),
					phys_state: "Disabled".into(),
					link_layer: "Ethernet".into(),
					rate_gbps: None,
				}],
			}],
		};
		assert!(!inv_down.any_active_roce(), "DOWN ports shouldn't count");

		assert!(!RdmaInventory::default().any_active_roce());
	}
}
