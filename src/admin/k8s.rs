// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Read-only views over the K8s objects accelrd's other subcommands
//! produce. We never write anything from the admin pod — its
//! ServiceAccount only has list/watch/get verbs (see manifests/rbac.yaml).

use std::collections::BTreeMap;

use anyhow::Context;
use k8s_openapi::api::core::v1::Node;
use kube::{Api, Client};
use serde::Serialize;

const TOPO_PREFIX: &str = "accel-topo.lunnova.dev/";
const ACCEL_PREFIX: &str = "accel.lunnova.dev/";
const TEST_PREFIX: &str = "accel-test.lunnova.dev/";

#[derive(Debug, Serialize)]
pub struct NodeView {
	pub name: String,
	pub rack: Option<String>,
	pub block: Option<String>,
	pub vendor_counts: BTreeMap<String, u64>,
	pub model_counts: BTreeMap<String, u64>,
	pub total_accelerators: Option<u64>,
	pub fabric_domains: u64,
	pub last_probe: Option<ProbeRecord>,
	pub conditions: Vec<NodeCondition>,
	pub ready: bool,
	pub schedulable: bool,
	pub kubelet_version: Option<String>,
	pub topo_labels: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct ProbeRecord {
	pub at: Option<String>,
	pub bandwidth_gbps: Option<f64>,
	pub partner: Option<String>,
	pub verdict: Option<String>,
}

impl ProbeRecord {
	fn is_empty(&self) -> bool {
		self.at.is_none() && self.bandwidth_gbps.is_none() && self.partner.is_none() && self.verdict.is_none()
	}
}

#[derive(Debug, Serialize)]
pub struct NodeCondition {
	pub kind: String,
	pub status: String,
	pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClusterSummary {
	pub nodes_total: usize,
	pub nodes_ready: usize,
	pub racks: BTreeMap<String, RackSummary>,
	pub probe_verdicts: BTreeMap<String, usize>,
	pub vendor_totals: BTreeMap<String, u64>,
}

#[derive(Debug, Serialize, Default)]
pub struct RackSummary {
	pub nodes: Vec<String>,
	pub healthy_probes: usize,
	pub failed_probes: usize,
}

pub async fn list_nodes(client: &Client) -> anyhow::Result<Vec<NodeView>> {
	let api: Api<Node> = Api::all(client.clone());
	let list = api.list(&Default::default()).await.context("list nodes")?;
	Ok(list.items.iter().map(NodeView::from_node).collect())
}

pub fn summarize(nodes: &[NodeView]) -> ClusterSummary {
	let mut summary = ClusterSummary {
		nodes_total: nodes.len(),
		nodes_ready: nodes.iter().filter(|n| n.ready).count(),
		racks: BTreeMap::new(),
		probe_verdicts: BTreeMap::new(),
		vendor_totals: BTreeMap::new(),
	};
	for n in nodes {
		if let Some(rack) = &n.rack {
			let entry = summary.racks.entry(rack.clone()).or_default();
			entry.nodes.push(n.name.clone());
			match n.last_probe.as_ref().and_then(|p| p.verdict.as_deref()) {
				Some("pass") | Some("ok") => entry.healthy_probes += 1,
				Some(_) => entry.failed_probes += 1,
				None => {}
			}
		}
		if let Some(p) = &n.last_probe
			&& let Some(v) = &p.verdict
		{
			*summary.probe_verdicts.entry(v.clone()).or_default() += 1;
		}
		for (vendor, count) in &n.vendor_counts {
			*summary.vendor_totals.entry(vendor.clone()).or_default() += count;
		}
	}
	summary
}

impl NodeView {
	fn from_node(node: &Node) -> Self {
		let name = node.metadata.name.clone().unwrap_or_default();
		let labels = node.metadata.labels.clone().unwrap_or_default();
		let annotations = node.metadata.annotations.clone().unwrap_or_default();

		let rack = labels.get("accel-topo.lunnova.dev/rack").cloned();
		let block = labels.get("accel-topo.lunnova.dev/block").cloned();
		let total_accelerators = labels.get("accel.lunnova.dev/total-count").and_then(|v| v.parse().ok());
		let fabric_domains = labels
			.get("accel-topo.lunnova.dev/fabric-domains-count")
			.and_then(|v| v.parse().ok())
			.unwrap_or(0);

		let vendor_counts = parse_count_labels(&labels, "accel.lunnova.dev/vendor.", ".count");
		let model_counts = parse_count_labels(&labels, "accel.lunnova.dev/model.", ".count");

		let probe = ProbeRecord {
			at: annotations.get("accel-test.lunnova.dev/last-rack-at").cloned(),
			bandwidth_gbps: annotations
				.get("accel-test.lunnova.dev/last-rack-bandwidth-gbps")
				.and_then(|v| v.parse().ok()),
			partner: annotations.get("accel-test.lunnova.dev/last-rack-partner").cloned(),
			verdict: annotations.get("accel-test.lunnova.dev/last-rack-verdict").cloned(),
		};
		let last_probe = if probe.is_empty() { None } else { Some(probe) };

		let mut conditions: Vec<NodeCondition> = Vec::new();
		let mut ready = false;
		if let Some(status) = &node.status
			&& let Some(conds) = &status.conditions
		{
			for c in conds {
				let status_str = c.status.clone();
				if c.type_ == "Ready" && status_str == "True" {
					ready = true;
				}
				conditions.push(NodeCondition {
					kind: c.type_.clone(),
					status: status_str,
					reason: c.reason.clone(),
				});
			}
		}

		let schedulable = !node.spec.as_ref().is_some_and(|s| s.unschedulable.unwrap_or(false));
		let kubelet_version = node
			.status
			.as_ref()
			.and_then(|s| s.node_info.as_ref())
			.map(|i| i.kubelet_version.clone());

		// Surface the full set of accelrd-managed labels for the UI's
		// "details" view without dumping every kubelet system label.
		let topo_labels: BTreeMap<String, String> = labels
			.into_iter()
			.filter(|(k, _)| k.starts_with(TOPO_PREFIX) || k.starts_with(ACCEL_PREFIX) || k.starts_with(TEST_PREFIX))
			.collect();

		Self {
			name,
			rack,
			block,
			vendor_counts,
			model_counts,
			total_accelerators,
			fabric_domains,
			last_probe,
			conditions,
			ready,
			schedulable,
			kubelet_version,
			topo_labels,
		}
	}
}

/// Parse `<prefix>{key}<suffix>` labels into a `{key: count}` map.
/// Example: `accel.lunnova.dev/vendor.nvidia.count = 8` → `{"nvidia": 8}`.
fn parse_count_labels(labels: &BTreeMap<String, String>, prefix: &str, suffix: &str) -> BTreeMap<String, u64> {
	let mut out = BTreeMap::new();
	for (k, v) in labels {
		if let Some(rest) = k.strip_prefix(prefix)
			&& let Some(key) = rest.strip_suffix(suffix)
			&& let Ok(count) = v.parse::<u64>()
		{
			out.insert(key.to_string(), count);
		}
	}
	out
}
