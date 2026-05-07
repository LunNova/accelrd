// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Build the K8s label/annotation set that publishes a node's
//! accelerator topology in a way topology-aware schedulers (Kueue TAS,
//! scheduler-plugins, Volcano) can consume. We don't claim a reserved
//! `topology.kubernetes.io/*` namespace beyond the well-known passthroughs
//! — custom keys live under `accel.lunnova.dev` and `accel-topo.lunnova.dev`.

use std::collections::BTreeMap;

use crate::sensors::Accelerator;

use super::NodeTopology;

#[derive(Debug, Clone, Default)]
pub struct LabelSet {
	pub labels: BTreeMap<String, String>,
	pub annotations: BTreeMap<String, String>,
}

impl LabelSet {
	pub fn pretty(&self) -> String {
		let mut out = String::from("labels:\n");
		for (k, v) in &self.labels {
			out.push_str(&format!("  {k} = {v}\n"));
		}
		out.push_str("annotations:\n");
		for (k, v) in &self.annotations {
			let preview = if v.len() > 80 { format!("{}…", &v[..80]) } else { v.clone() };
			out.push_str(&format!("  {k} = {preview}\n"));
		}
		out
	}
}

pub fn build(topology: &NodeTopology, accelerators: &[Accelerator]) -> LabelSet {
	let mut set = LabelSet::default();

	if let Some(v) = &topology.block {
		set.labels.insert("accel-topo.lunnova.dev/block".into(), v.clone());
	}
	if let Some(v) = &topology.rack {
		set.labels.insert("accel-topo.lunnova.dev/rack".into(), v.clone());
	}

	// Fabric-domain presence labels — one per domain, value = member count.
	let mut domain_counts: BTreeMap<&str, usize> = BTreeMap::new();
	for d in &topology.fabric_domains {
		*domain_counts.entry(d.id.as_str()).or_default() += d.member_accelerators.len();
	}
	for (id, count) in &domain_counts {
		set.labels.insert(format!("accel-topo.lunnova.dev/fabric-domain.{id}"), count.to_string());
	}
	set.labels
		.insert("accel-topo.lunnova.dev/fabric-domains-count".into(), topology.fabric_domains.len().to_string());

	// Vendor counts.
	let mut vendor_counts: BTreeMap<String, usize> = BTreeMap::new();
	let mut model_counts: BTreeMap<String, usize> = BTreeMap::new();
	let mut memkind_counts: BTreeMap<String, usize> = BTreeMap::new();
	let mut physical_funcs: BTreeMap<String, std::collections::BTreeSet<std::path::PathBuf>> = BTreeMap::new();

	for a in accelerators {
		*vendor_counts.entry(a.id.vendor.slug().to_string()).or_default() += 1;
		*model_counts.entry(slugify(&a.model)).or_default() += 1;
		*memkind_counts.entry(a.memory_kind.slug().to_string()).or_default() += 1;
		if let Some(parent) = a.device_dir.parent() {
			physical_funcs.entry(a.id.vendor.slug().into()).or_default().insert(parent.to_path_buf());
		}
	}
	for (k, c) in &vendor_counts {
		set.labels.insert(format!("accel.lunnova.dev/vendor.{k}.count"), c.to_string());
	}
	for (k, c) in &model_counts {
		set.labels.insert(format!("accel.lunnova.dev/model.{k}.count"), c.to_string());
	}
	for (k, c) in &memkind_counts {
		set.labels.insert(format!("accel.lunnova.dev/memory-kind.{k}.count"), c.to_string());
	}
	for (k, funcs) in &physical_funcs {
		set.labels.insert(format!("accel.lunnova.dev/vendor.{k}.physical-count"), funcs.len().to_string());
	}
	set.labels.insert("accel.lunnova.dev/total-count".into(), accelerators.len().to_string());

	// Inventory + fabric-graph annotations.
	if let Ok(inv) = serde_json::to_string(&accelerators.iter().map(InventoryEntry::from).collect::<Vec<_>>()) {
		set.annotations.insert("accel.lunnova.dev/inventory".into(), inv);
	}
	if let Ok(fg) = serde_json::to_string(&topology.fabric_domains) {
		set.annotations.insert("accel.lunnova.dev/fabric-graph".into(), fg);
	}

	set
}

#[derive(serde::Serialize)]
struct InventoryEntry<'a> {
	vendor: String,
	model: &'a str,
	memory_kind: &'a str,
	memory_total_bytes: Option<u64>,
	fabric_domain: Option<&'a str>,
	numa_node: Option<i32>,
	accel_index: u32,
	pci_addr: &'a str,
	coverage: &'a str,
	partitioned: bool,
}

impl<'a> From<&'a Accelerator> for InventoryEntry<'a> {
	fn from(a: &'a Accelerator) -> Self {
		Self {
			vendor: a.id.vendor.slug().into(),
			model: &a.model,
			memory_kind: a.memory_kind.slug(),
			memory_total_bytes: a.memory_total_bytes,
			fabric_domain: a.fabric_domain.as_deref(),
			numa_node: a.numa_node,
			accel_index: a.id.drm_index,
			pci_addr: &a.id.pci_addr,
			coverage: a.coverage.slug(),
			partitioned: a.partitioned,
		}
	}
}

/// K8s label values must be lowercase alphanumerics, dashes, dots, and
/// underscores. Slugify model names accordingly. Squashes runs of
/// non-allowed chars to a single dash.
fn slugify(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut last_dash = false;
	for c in s.chars() {
		let c = c.to_ascii_lowercase();
		if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
			out.push(c);
			last_dash = false;
		} else if !last_dash {
			out.push('-');
			last_dash = true;
		}
	}
	let trimmed = out.trim_matches('-').to_string();
	if trimmed.is_empty() { "unknown".into() } else { trimmed }
}
