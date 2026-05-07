// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Build the K8s label/annotation set that publishes a node's
//! accelerator topology in a way topology-aware schedulers (Kueue TAS,
//! scheduler-plugins, Volcano) can consume. We don't claim a reserved
//! `topology.kubernetes.io/*` namespace beyond the well-known passthroughs
//! — custom keys live under `accel.lunnova.dev` and `accel-topo.lunnova.dev`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::sensors::{Accelerator, common};

use super::NodeTopology;

#[derive(Debug, Clone, Default)]
pub struct LabelSet {
	pub labels: BTreeMap<String, String>,
	pub annotations: BTreeMap<String, String>,
}

impl LabelSet {
	pub fn pretty(&self) -> String {
		use std::fmt::Write as _;
		let mut out = String::from("labels:\n");
		for (k, v) in &self.labels {
			let _ = writeln!(out, "  {k} = {v}");
		}
		out.push_str("annotations:\n");
		for (k, v) in &self.annotations {
			let preview = if v.len() > 80 { format!("{}…", &v[..80]) } else { v.clone() };
			let _ = writeln!(out, "  {k} = {preview}");
		}
		out
	}
}

pub fn build(topology: &NodeTopology, accelerators: &[Accelerator]) -> LabelSet {
	let mut set = LabelSet::default();

	// Topology block/rack passthroughs.
	if let Some(v) = &topology.block {
		set.labels.insert("accel-topo.lunnova.dev/block".into(), v.clone());
	}
	if let Some(v) = &topology.rack {
		set.labels.insert("accel-topo.lunnova.dev/rack".into(), v.clone());
	}

	// Fabric-domain presence: one count per domain id.
	let mut domain_counts: BTreeMap<&str, usize> = BTreeMap::new();
	for d in &topology.fabric_domains {
		*domain_counts.entry(d.id.as_str()).or_default() += d.member_accelerators.len();
	}
	for (id, count) in &domain_counts {
		set.labels.insert(format!("accel-topo.lunnova.dev/fabric-domain.{id}"), count.to_string());
	}
	set.labels
		.insert("accel-topo.lunnova.dev/fabric-domains-count".into(), topology.fabric_domains.len().to_string());

	// Per-vendor / per-model / per-memory-kind counts.
	let vendor_counts = count_by(accelerators, |a| a.id.vendor.slug().to_string());
	let model_counts = count_by(accelerators, |a| slugify(&a.model));
	let memkind_counts = count_by(accelerators, |a| a.memory_kind.slug().to_string());
	emit_count_labels(&mut set, "accel.lunnova.dev/vendor", &vendor_counts);
	emit_count_labels(&mut set, "accel.lunnova.dev/model", &model_counts);
	emit_count_labels(&mut set, "accel.lunnova.dev/memory-kind", &memkind_counts);

	// Physical card count per vendor: collapse partitions/SR-IOV by counting
	// distinct parent PCI functions.
	let mut physical_funcs: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
	for a in accelerators {
		if let Some(parent) = a.device_dir.parent() {
			physical_funcs.entry(a.id.vendor.slug().into()).or_default().insert(parent.to_path_buf());
		}
	}
	for (vendor, funcs) in &physical_funcs {
		set.labels.insert(format!("accel.lunnova.dev/vendor.{vendor}.physical-count"), funcs.len().to_string());
	}
	set.labels.insert("accel.lunnova.dev/total-count".into(), accelerators.len().to_string());

	// SR-IOV: surface configured + total VF capacity per physical card.
	// Only emitted when the card actually exposes the sysfs files.
	let mut sriov_total = 0u64;
	let mut sriov_used = 0u64;
	let mut sriov_seen = false;
	for a in accelerators {
		if let Some(parent) = a.device_dir.parent()
			&& let Some((total, used)) = common::sriov_capacity(parent)
		{
			sriov_total = sriov_total.max(total);
			sriov_used += used;
			sriov_seen = true;
		}
	}
	if sriov_seen {
		set.labels.insert("accel.lunnova.dev/sriov.total".into(), sriov_total.to_string());
		set.labels.insert("accel.lunnova.dev/sriov.configured".into(), sriov_used.to_string());
	}

	// Inventory + fabric-graph annotations. Errors here are non-fatal: a
	// serialize failure just means the annotation is missing.
	let inventory: Vec<InventoryEntry> = accelerators.iter().map(InventoryEntry::from).collect();
	if let Ok(s) = serde_json::to_string(&inventory) {
		set.annotations.insert("accel.lunnova.dev/inventory".into(), s);
	}
	if let Ok(s) = serde_json::to_string(&topology.fabric_domains) {
		set.annotations.insert("accel.lunnova.dev/fabric-graph".into(), s);
	}

	// LLDP — one label per observed switch chassis (so multi-uplink
	// hosts plugged into different TORs surface that fact), plus a
	// JSON annotation with full per-interface neighbour detail.
	if !topology.lldp_neighbors.is_empty() {
		let mut chassis_seen: BTreeSet<&str> = BTreeSet::new();
		for n in &topology.lldp_neighbors {
			let slug = n.rack_slug();
			if chassis_seen.insert(n.chassis_id.as_str()) {
				set.labels.insert(format!("accel-topo.lunnova.dev/lldp.chassis.{slug}"), "1".into());
			}
		}
		set.labels.insert("accel-topo.lunnova.dev/lldp.chassis-count".into(), chassis_seen.len().to_string());
		if let Ok(s) = serde_json::to_string(&topology.lldp_neighbors) {
			set.annotations.insert("accel-topo.lunnova.dev/lldp-neighbors".into(), s);
		}
	}

	set
}

fn count_by<F: Fn(&Accelerator) -> String>(accels: &[Accelerator], key: F) -> BTreeMap<String, usize> {
	let mut counts: BTreeMap<String, usize> = BTreeMap::new();
	for a in accels {
		*counts.entry(key(a)).or_default() += 1;
	}
	counts
}

fn emit_count_labels(set: &mut LabelSet, prefix: &str, counts: &BTreeMap<String, usize>) {
	for (key, count) in counts {
		set.labels.insert(format!("{prefix}.{key}.count"), count.to_string());
	}
}

#[derive(serde::Serialize)]
struct InventoryEntry<'a> {
	vendor: &'static str,
	model: &'a str,
	memory_kind: &'static str,
	memory_total_bytes: Option<u64>,
	fabric_domain: Option<&'a str>,
	numa_node: Option<i32>,
	accel_index: u32,
	pci_addr: &'a str,
	coverage: &'static str,
	partitioned: bool,
}

impl<'a> From<&'a Accelerator> for InventoryEntry<'a> {
	fn from(a: &'a Accelerator) -> Self {
		Self {
			vendor: a.id.vendor.slug(),
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
	let trimmed = out.trim_matches('-');
	if trimmed.is_empty() { "unknown".into() } else { trimmed.to_string() }
}
