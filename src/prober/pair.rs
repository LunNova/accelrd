// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Group nodes by rack label and pick the next pair to probe. The
//! "next pair" rule:
//!
//!  1. Within a rack, pick the node with the oldest `last-rack-at`
//!     annotation (None counts as "infinitely old").
//!  2. From the remaining nodes, pick the next-oldest as the partner.
//!  3. Tie-break by node name (stable, deterministic, lexicographic).
//!
//! This is per-node LRU rather than per-pair LRU — the latter would
//! require N²-many timestamps. Per-node coverage still guarantees full
//! pair-coverage in `O(N²)` cycles since the most-stale node will always
//! get pulled into a probe.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Node;

use crate::prober::{ANN_LAST_AT, ANN_LB_LAST_AT};

const RACK_LABEL: &str = "accel-topo.lunnova.dev/rack";
const RDMA_LABEL: &str = "accel-net.lunnova.dev/rdma";
const RDMA_PRESENT: &str = "present";
const NODE_READY_TYPE: &str = "Ready";
const NODE_READY_STATUS: &str = "True";

#[derive(Debug, Clone)]
pub struct NodeView {
	pub name: String,
	/// Last cross-node pair-probe timestamp (`last-rack-at`). Drives
	/// `pick_pair`'s LRU + cadence guard.
	pub last_at: Option<String>,
	/// Last single-node loopback-probe timestamp (`last-loopback-at`).
	/// Drives `pick_loopback`'s cadence guard. Independent from
	/// `last_at` — a node accumulates both kinds over time.
	pub last_loopback_at: Option<String>,
	pub rack: String,
}

pub fn eligible(nodes: &[Node]) -> Vec<NodeView> {
	nodes
		.iter()
		.filter(|n| is_ready(n))
		.filter_map(NodeView::from_node)
		.collect()
}

pub fn group_by_rack(nodes: Vec<NodeView>) -> BTreeMap<String, Vec<NodeView>> {
	let mut out: BTreeMap<String, Vec<NodeView>> = BTreeMap::new();
	for n in nodes {
		out.entry(n.rack.clone()).or_default().push(n);
	}
	for v in out.values_mut() {
		v.sort_by(|a, b| a.name.cmp(&b.name));
	}
	out
}

/// Pick a singleton-rack node for a single-host loopback self-test, or
/// `None` if either: the rack has more than one RDMA-capable member
/// (use `pick_pair` instead), the rack has zero, or the lone member's
/// last loopback was more recent than the cadence cutoff.
pub fn pick_loopback<'a>(members: &'a [NodeView], cadence_cutoff_rfc3339: &str) -> Option<&'a NodeView> {
	if members.len() != 1 {
		return None;
	}
	let only = &members[0];
	if let Some(at) = only.last_loopback_at.as_deref()
		&& at >= cadence_cutoff_rfc3339
	{
		return None;
	}
	Some(only)
}

/// Pick the next pair to probe in a rack, or `None` if the rack was
/// probed within `cadence_cutoff_rfc3339` (i.e. the LRU pair's "older"
/// timestamp is more recent than the cutoff).
pub fn pick_pair<'a>(members: &'a [NodeView], cadence_cutoff_rfc3339: &str) -> Option<(&'a NodeView, &'a NodeView)> {
	if members.len() < 2 {
		return None;
	}
	let mut sorted: Vec<&NodeView> = members.iter().collect();
	sorted.sort_by(|a, b| match (a.last_at.as_deref(), b.last_at.as_deref()) {
		(None, None) => a.name.cmp(&b.name),
		(None, Some(_)) => std::cmp::Ordering::Less,
		(Some(_), None) => std::cmp::Ordering::Greater,
		(Some(x), Some(y)) => x.cmp(y).then(a.name.cmp(&b.name)),
	});
	let oldest = sorted[0];
	let next = sorted[1];
	// Cadence guard: if even the oldest of the pair is more recent than
	// the cutoff, skip — both members were touched within cadence.
	let pair_oldest = match (oldest.last_at.as_deref(), next.last_at.as_deref()) {
		(None, _) | (_, None) => return Some((oldest, next)),
		(Some(a), Some(b)) => a.min(b),
	};
	if pair_oldest >= cadence_cutoff_rfc3339 {
		return None;
	}
	Some((oldest, next))
}

impl NodeView {
	fn from_node(n: &Node) -> Option<Self> {
		let name = n.metadata.name.clone()?;
		let labels = n.metadata.labels.as_ref()?;
		let rack = labels.get(RACK_LABEL)?.clone();
		// Skip nodes that can't run an ibverbs probe at all — neither
		// a hardware HCA (mlx5_*) nor soft-RoCE (rxe*) is loaded. The
		// daemon's RDMA scan covers both, so this single label is the
		// right "loopback-capable" gate; pairing or self-testing such
		// a node would just produce a guaranteed-fail probe.
		if labels.get(RDMA_LABEL).map(String::as_str) != Some(RDMA_PRESENT) {
			return None;
		}
		let annotations = n.metadata.annotations.as_ref();
		let last_at = annotations.and_then(|a| a.get(ANN_LAST_AT)).cloned();
		let last_loopback_at = annotations.and_then(|a| a.get(ANN_LB_LAST_AT)).cloned();
		Some(Self {
			name,
			last_at,
			last_loopback_at,
			rack,
		})
	}
}

fn is_ready(node: &Node) -> bool {
	let Some(status) = node.status.as_ref() else {
		return false;
	};
	let Some(conditions) = status.conditions.as_ref() else {
		return false;
	};
	conditions
		.iter()
		.any(|c| c.type_ == NODE_READY_TYPE && c.status == NODE_READY_STATUS)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn nv(name: &str, last: Option<&str>) -> NodeView {
		NodeView {
			name: name.into(),
			last_at: last.map(str::to_string),
			last_loopback_at: None,
			rack: "r1".into(),
		}
	}

	fn nv_loopback(name: &str, last_loopback: Option<&str>) -> NodeView {
		NodeView {
			name: name.into(),
			last_at: None,
			last_loopback_at: last_loopback.map(str::to_string),
			rack: "r1".into(),
		}
	}

	#[test]
	fn picks_oldest_two_when_all_unprobed() {
		let m = vec![nv("a", None), nv("b", None), nv("c", None)];
		let pair = pick_pair(&m, "1900-01-01T00:00:00Z").unwrap();
		assert_eq!(pair.0.name, "a");
		assert_eq!(pair.1.name, "b");
	}

	#[test]
	fn prefers_unprobed_over_probed() {
		let m = vec![
			nv("a", Some("2026-05-07T00:00:00Z")),
			nv("b", Some("2026-05-07T00:00:00Z")),
			nv("c", None),
		];
		let pair = pick_pair(&m, "1900-01-01T00:00:00Z").unwrap();
		assert_eq!(pair.0.name, "c");
		// next-oldest is whichever sorts first alphabetically among the timestamped
		assert_eq!(pair.1.name, "a");
	}

	#[test]
	fn picks_oldest_pair_when_all_probed() {
		let m = vec![
			nv("a", Some("2026-05-01T00:00:00Z")),
			nv("b", Some("2026-05-07T00:00:00Z")),
			nv("c", Some("2026-05-03T00:00:00Z")),
		];
		// cutoff is newer than a and c, but older than b — so the (a,c)
		// pair is eligible and the function returns it.
		let pair = pick_pair(&m, "2026-05-05T00:00:00Z").unwrap();
		assert_eq!(pair.0.name, "a");
		assert_eq!(pair.1.name, "c");
	}

	#[test]
	fn returns_none_when_within_cadence() {
		let m = vec![
			nv("a", Some("2026-05-07T12:00:00Z")),
			nv("b", Some("2026-05-07T12:00:00Z")),
		];
		// cutoff is older than both => both within cadence => skip
		assert!(pick_pair(&m, "2026-05-07T11:00:00Z").is_none());
	}

	#[test]
	fn returns_none_for_singleton_rack() {
		let m = vec![nv("a", None)];
		assert!(pick_pair(&m, "1900-01-01T00:00:00Z").is_none());
	}

	#[test]
	fn loopback_picks_singleton() {
		let m = vec![nv_loopback("a", None)];
		let picked = pick_loopback(&m, "1900-01-01T00:00:00Z").unwrap();
		assert_eq!(picked.name, "a");
	}

	#[test]
	fn loopback_skips_when_pair_is_possible() {
		let m = vec![nv_loopback("a", None), nv_loopback("b", None)];
		assert!(pick_loopback(&m, "1900-01-01T00:00:00Z").is_none());
	}

	#[test]
	fn loopback_skips_within_cadence() {
		let m = vec![nv_loopback("a", Some("2026-05-07T12:00:00Z"))];
		// cutoff is older than the recorded probe → still within cadence
		assert!(pick_loopback(&m, "2026-05-07T11:00:00Z").is_none());
	}

	#[test]
	fn loopback_picks_when_past_cadence() {
		let m = vec![nv_loopback("a", Some("2026-05-01T00:00:00Z"))];
		// cutoff is newer than the recorded probe → past cadence, eligible
		let picked = pick_loopback(&m, "2026-05-05T00:00:00Z").unwrap();
		assert_eq!(picked.name, "a");
	}
}
