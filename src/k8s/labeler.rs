// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Node labeler. Two modes:
//! - Disabled: prints intended labels at info level on first reconcile
//!   (debug afterwards). What you get on a dev box.
//! - Active: real `Api<Node>::patch` against the in-cluster API server,
//!   using a JSON merge patch that adds/updates our managed keys *and*
//!   nulls out any of our managed prefixes that are no longer present
//!   (so the node converges as the daemon's view changes).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use kube::api::{Patch, PatchParams};
use kube::{Api, Client};
use serde_json::{Map, Value, json};

use crate::topology::labels::LabelSet;

/// Label/annotation key prefixes the daemon owns. Anything *not* under
/// one of these is left alone during reconcile.
const MANAGED_PREFIXES: &[&str] = &[
	"accel.lunnova.dev/",
	"accel-topo.lunnova.dev/",
	"accel-ready.lunnova.dev/",
	"accel-net.lunnova.dev/",
];

pub struct OptionalLabeler {
	state: LabelerState,
	node_name: String,
	announced: AtomicBool,
}

enum LabelerState {
	Disabled,
	Active(Client),
}

impl OptionalLabeler {
	/// Construct. When `enabled` is true, attempts to build an in-cluster
	/// kube Client. Failure (no SA token, network issue) degrades to
	/// `Disabled` rather than aborting startup — observability data
	/// still flows, just no node labels get patched.
	pub async fn new(enabled: bool, node_name: String) -> Self {
		let state = if enabled {
			match Client::try_default().await {
				Ok(client) => {
					tracing::info!(node = %node_name, "kube client initialized; labeler active");
					LabelerState::Active(client)
				}
				Err(e) => {
					tracing::warn!(
						error = %e,
						"kube client init failed; labeler degrading to disabled (telemetry continues)",
					);
					LabelerState::Disabled
				}
			}
		} else {
			LabelerState::Disabled
		};
		Self {
			state,
			node_name,
			announced: AtomicBool::new(false),
		}
	}

	pub async fn reconcile(&self, labels: &LabelSet) {
		let first = !self.announced.swap(true, Ordering::Relaxed);
		match &self.state {
			LabelerState::Disabled => self.log_intended(labels, first),
			LabelerState::Active(client) => self.apply(client, labels, first).await,
		}
	}

	fn log_intended(&self, labels: &LabelSet, first: bool) {
		if first {
			tracing::info!(
				node = %self.node_name,
				labels = labels.labels.len(),
				annotations = labels.annotations.len(),
				"k8s labeler disabled (no in-cluster SA or --no-k8s); intended labels:\n{}",
				labels.pretty(),
			);
		} else {
			tracing::debug!(
				node = %self.node_name,
				labels = labels.labels.len(),
				"would-reconcile (k8s disabled)",
			);
		}
	}

	async fn apply(&self, client: &Client, labels: &LabelSet, first: bool) {
		let nodes: Api<Node> = Api::all(client.clone());

		// Read current node so we can null out stale managed keys.
		let current = match nodes.get_metadata(&self.node_name).await {
			Ok(n) => n,
			Err(e) => {
				tracing::warn!(error = %e, node = %self.node_name, "node metadata GET failed; skipping reconcile");
				return;
			}
		};

		let patch_body = build_merge_patch(
			current.metadata.labels.as_ref(),
			current.metadata.annotations.as_ref(),
			labels,
		);
		let patch_params = PatchParams::default();
		let patch = Patch::Merge(&patch_body);

		const MAX_ATTEMPTS: u32 = 3;
		for attempt in 1..=MAX_ATTEMPTS {
			match nodes.patch(&self.node_name, &patch_params, &patch).await {
				Ok(_) => {
					if first {
						tracing::info!(
							node = %self.node_name,
							labels = labels.labels.len(),
							annotations = labels.annotations.len(),
							"node labels patched (first reconcile)",
						);
					} else {
						tracing::debug!(node = %self.node_name, "node labels patched");
					}
					return;
				}
				Err(e) if attempt < MAX_ATTEMPTS => {
					let backoff = Duration::from_millis(200u64 << attempt);
					tracing::debug!(error = %e, attempt, "node patch failed; retrying");
					tokio::time::sleep(backoff).await;
				}
				Err(e) => {
					tracing::warn!(
						error = %e,
						node = %self.node_name,
						attempts = attempt,
						"node patch failed after retries; will retry next reconcile",
					);
					return;
				}
			}
		}
	}
}

/// Construct a JSON merge patch (RFC 7396) for `metadata.labels` and
/// `metadata.annotations`. Keys we manage that aren't in `desired` are
/// set to `null` so the apiserver removes them — this matters when the
/// hardware on a node changes (a card is hot-removed, a model is
/// replaced) and we need stale labels to disappear.
fn build_merge_patch(
	current_labels: Option<&BTreeMap<String, String>>,
	current_annotations: Option<&BTreeMap<String, String>>,
	desired: &LabelSet,
) -> Value {
	let labels = merged_section(current_labels, &desired.labels);
	let annotations = merged_section(current_annotations, &desired.annotations);
	json!({ "metadata": { "labels": labels, "annotations": annotations } })
}

/// Build one section (labels or annotations) of the merge patch: null
/// out managed-prefix keys we no longer want, then overlay the desired
/// keys.
fn merged_section(
	current: Option<&BTreeMap<String, String>>,
	desired: &BTreeMap<String, String>,
) -> Map<String, Value> {
	let mut out = Map::new();
	if let Some(cur) = current {
		for k in cur.keys() {
			if is_managed(k) && !desired.contains_key(k) {
				out.insert(k.clone(), Value::Null);
			}
		}
	}
	for (k, v) in desired {
		out.insert(k.clone(), Value::String(v.clone()));
	}
	out
}

fn is_managed(key: &str) -> bool {
	MANAGED_PREFIXES.iter().any(|p| key.starts_with(p))
}
