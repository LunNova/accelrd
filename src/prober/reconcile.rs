// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! One reconcile pass: list nodes → group by rack → pick a pair (or a
//! singleton-rack loopback target) → orchestrate the two-pod probe →
//! patch annotations. Designed to be called on a timer; idempotent
//! against partial state because each probe is its own pair-id-tagged
//! Pod set, and stale tags get cleaned up at the start of the next pass.

use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::Client;
use kube::api::{Api, ListParams};
use opentelemetry::KeyValue;
use opentelemetry::global;

use crate::prober::Resolved;
use crate::prober::annotations::{Verdict, patch_loopback, patch_pair};
use crate::prober::pair::{NodeView, eligible, group_by_rack, pick_loopback, pick_pair};
use crate::prober::pods;
use crate::time::now_minus_secs;

pub async fn run_once(client: &Client, args: &Resolved) {
	let started = Instant::now();
	let nodes_api: Api<Node> = Api::all(client.clone());
	let pods_api: Api<Pod> = Api::namespaced(client.clone(), &args.namespace);

	pods::cleanup_orphans(&pods_api, Duration::from_secs(args.test_duration_secs * 4 + 600)).await;

	let nodes = match nodes_api.list(&ListParams::default()).await {
		Ok(l) => l.items,
		Err(e) => {
			tracing::warn!(error = %e, "node list failed");
			return;
		}
	};
	let cutoff = now_minus_secs()(args.cadence_secs);
	let racks = group_by_rack(eligible(&nodes));

	let mut tasks = Vec::new();
	let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(args.max_concurrent_pairs as usize));
	for (rack, members) in racks {
		// Multi-member rack: cross-node pair probe. Singleton: loopback
		// self-test (server + client both pinned to the same node).
		// Mutually exclusive: pick_pair requires ≥2 members,
		// pick_loopback requires exactly 1.
		if let Some(pair) = pick_pair(&members, &cutoff) {
			let (server_view, client_view) = (pair.0.clone(), pair.1.clone());
			let Ok(permit) = semaphore.clone().acquire_owned().await else {
				break;
			};
			let nodes_api = nodes_api.clone();
			let pods_api = pods_api.clone();
			let args = args.clone();
			let rack_id = rack.clone();
			tasks.push(tokio::spawn(async move {
				let _permit = permit;
				probe_pair(nodes_api, pods_api, args, rack_id, server_view, client_view).await;
			}));
		} else if let Some(node) = pick_loopback(&members, &cutoff) {
			let node_view = node.clone();
			let Ok(permit) = semaphore.clone().acquire_owned().await else {
				break;
			};
			let nodes_api = nodes_api.clone();
			let pods_api = pods_api.clone();
			let args = args.clone();
			let rack_id = rack.clone();
			tasks.push(tokio::spawn(async move {
				let _permit = permit;
				probe_loopback(nodes_api, pods_api, args, rack_id, node_view).await;
			}));
		}
	}
	for t in tasks {
		let _ = t.await;
	}

	emit_cycle_metric(started.elapsed());
	tracing::info!(
		elapsed_ms = started.elapsed().as_millis() as u64,
		"reconcile cycle complete"
	);
}

async fn probe_pair(
	nodes_api: Api<Node>,
	pods_api: Api<Pod>,
	args: Resolved,
	rack: String,
	server: NodeView,
	client: NodeView,
) {
	let pair_id = pair_tag(&server.name, &client.name);
	let span =
		tracing::info_span!("probe_pair", rack = %rack, server = %server.name, client = %client.name, pair = %pair_id);
	let _enter = span.enter();
	tracing::info!("starting paired probe");

	let server_pod_spec = pods::PodSpec {
		role: pods::Role::Server,
		node_name: &server.name,
		partner_node: &client.name,
		partner_ip: None,
		rack: &rack,
		pair_id: &pair_id,
		args: &args,
	};

	let server_pod = match pods::build_pod(&server_pod_spec) {
		Ok(p) => p,
		Err(e) => {
			emit_attempt_metric(&rack, "fail");
			patch_pair(
				&nodes_api,
				&server.name,
				&client.name,
				&Verdict::Fail(&format!("build server pod: {e}")),
			)
			.await;
			return;
		}
	};

	let server_created = match pods::create(&pods_api, &server_pod).await {
		Ok(p) => p,
		Err(e) => {
			emit_attempt_metric(&rack, "fail");
			patch_pair(
				&nodes_api,
				&server.name,
				&client.name,
				&Verdict::Fail(&format!("create server pod: {e}")),
			)
			.await;
			return;
		}
	};
	let server_pod_name = server_created.metadata.name.clone().unwrap_or_default();

	// Wait for the CNI to assign the server pod its IP. Without this,
	// we'd race with kubelet and kick off the client pod pointing at
	// nothing. 60s is plenty for any sane CNI; image-pull delays are
	// the dominant cost and they're absorbed here.
	let server_pod_ip = match pods::await_pod_ip(&pods_api, &server_pod_name, Duration::from_secs(60)).await {
		Ok(ip) => ip,
		Err(e) => {
			pods::delete(&pods_api, &server_pod_name).await;
			emit_attempt_metric(&rack, "fail");
			patch_pair(
				&nodes_api,
				&server.name,
				&client.name,
				&Verdict::Fail(&format!("server pod IP: {e}")),
			)
			.await;
			return;
		}
	};

	let client_pod_spec = pods::PodSpec {
		role: pods::Role::Client,
		node_name: &client.name,
		partner_node: &server.name,
		partner_ip: Some(&server_pod_ip),
		rack: &rack,
		pair_id: &pair_id,
		args: &args,
	};
	let client_pod = match pods::build_pod(&client_pod_spec) {
		Ok(p) => p,
		Err(e) => {
			pods::delete(&pods_api, &server_pod_name).await;
			emit_attempt_metric(&rack, "fail");
			patch_pair(
				&nodes_api,
				&server.name,
				&client.name,
				&Verdict::Fail(&format!("build client pod: {e}")),
			)
			.await;
			return;
		}
	};

	let client_created = match pods::create(&pods_api, &client_pod).await {
		Ok(p) => p,
		Err(e) => {
			pods::delete(&pods_api, &server_pod_name).await;
			emit_attempt_metric(&rack, "fail");
			patch_pair(
				&nodes_api,
				&server.name,
				&client.name,
				&Verdict::Fail(&format!("create client pod: {e}")),
			)
			.await;
			return;
		}
	};
	let client_pod_name = client_created.metadata.name.clone().unwrap_or_default();

	let total_timeout = Duration::from_secs(args.test_duration_secs + 120);
	let phase = match pods::await_terminal(&pods_api, &client_pod_name, total_timeout).await {
		Ok(p) => p,
		Err(e) => {
			pods::delete(&pods_api, &server_pod_name).await;
			pods::delete(&pods_api, &client_pod_name).await;
			emit_attempt_metric(&rack, "fail");
			patch_pair(
				&nodes_api,
				&server.name,
				&client.name,
				&Verdict::Fail(&format!("watch client pod: {e}")),
			)
			.await;
			return;
		}
	};

	let verdict = match phase {
		pods::Phase::Succeeded => match pods::read_record(&pods_api, &client_pod_name).await {
			Ok(r) if r.ok => {
				emit_attempt_metric(&rack, "ok");
				patch_pair(&nodes_api, &server.name, &client.name, &Verdict::Ok(&r)).await;
				return cleanup(&pods_api, &server_pod_name, &client_pod_name).await;
			}
			Ok(r) => Verdict::Fail(&r.message.clone().unwrap_or_else(|| "probe ok=false".into())).to_owned_msg(),
			Err(e) => OwnedVerdict::Fail(format!("read client pod: {e}")),
		},
		pods::Phase::Failed => OwnedVerdict::Fail("client pod phase Failed".into()),
		pods::Phase::Timeout => OwnedVerdict::Timeout,
	};

	emit_attempt_metric(&rack, verdict.label());
	patch_pair(&nodes_api, &server.name, &client.name, &verdict.borrow()).await;
	cleanup(&pods_api, &server_pod_name, &client_pod_name).await;
}

async fn cleanup(pods_api: &Api<Pod>, server: &str, client: &str) {
	pods::delete(pods_api, client).await;
	pods::delete(pods_api, server).await;
}

/// Single-host loopback variant of `probe_pair`. Spawns server + client
/// pods both pinned to the same node via `nodeName`. The actual data
/// path is HCA-internal (mlx5 self-loopback) or in-software (rxe), so
/// the verdict is a "this node's verbs stack works" smoke test, not a
/// fabric measurement. Bandwidth numbers can read unrealistically high
/// because the wire isn't touched — that's expected and is why the
/// admin verdict pill explicitly tags the source as `loopback`.
async fn probe_loopback(nodes_api: Api<Node>, pods_api: Api<Pod>, args: Resolved, rack: String, node: NodeView) {
	let pair_id = pair_tag(&node.name, &node.name);
	let span = tracing::info_span!("probe_loopback", rack = %rack, node = %node.name, pair = %pair_id);
	let _enter = span.enter();
	tracing::info!("starting loopback probe");

	let server_pod_spec = pods::PodSpec {
		role: pods::Role::Server,
		node_name: &node.name,
		partner_node: &node.name,
		partner_ip: None,
		rack: &rack,
		pair_id: &pair_id,
		args: &args,
	};
	let server_pod = match pods::build_pod(&server_pod_spec) {
		Ok(p) => p,
		Err(e) => {
			emit_attempt_metric_loopback(&rack, "fail");
			patch_loopback(
				&nodes_api,
				&node.name,
				&Verdict::Fail(&format!("build server pod: {e}")),
			)
			.await;
			return;
		}
	};
	let server_created = match pods::create(&pods_api, &server_pod).await {
		Ok(p) => p,
		Err(e) => {
			emit_attempt_metric_loopback(&rack, "fail");
			patch_loopback(
				&nodes_api,
				&node.name,
				&Verdict::Fail(&format!("create server pod: {e}")),
			)
			.await;
			return;
		}
	};
	let server_pod_name = server_created.metadata.name.clone().unwrap_or_default();

	let server_pod_ip = match pods::await_pod_ip(&pods_api, &server_pod_name, Duration::from_secs(60)).await {
		Ok(ip) => ip,
		Err(e) => {
			pods::delete(&pods_api, &server_pod_name).await;
			emit_attempt_metric_loopback(&rack, "fail");
			patch_loopback(&nodes_api, &node.name, &Verdict::Fail(&format!("server pod IP: {e}"))).await;
			return;
		}
	};

	let client_pod_spec = pods::PodSpec {
		role: pods::Role::Client,
		node_name: &node.name,
		partner_node: &node.name,
		partner_ip: Some(&server_pod_ip),
		rack: &rack,
		pair_id: &pair_id,
		args: &args,
	};
	let client_pod = match pods::build_pod(&client_pod_spec) {
		Ok(p) => p,
		Err(e) => {
			pods::delete(&pods_api, &server_pod_name).await;
			emit_attempt_metric_loopback(&rack, "fail");
			patch_loopback(
				&nodes_api,
				&node.name,
				&Verdict::Fail(&format!("build client pod: {e}")),
			)
			.await;
			return;
		}
	};
	let client_created = match pods::create(&pods_api, &client_pod).await {
		Ok(p) => p,
		Err(e) => {
			pods::delete(&pods_api, &server_pod_name).await;
			emit_attempt_metric_loopback(&rack, "fail");
			patch_loopback(
				&nodes_api,
				&node.name,
				&Verdict::Fail(&format!("create client pod: {e}")),
			)
			.await;
			return;
		}
	};
	let client_pod_name = client_created.metadata.name.clone().unwrap_or_default();

	let total_timeout = Duration::from_secs(args.test_duration_secs + 120);
	let phase = match pods::await_terminal(&pods_api, &client_pod_name, total_timeout).await {
		Ok(p) => p,
		Err(e) => {
			pods::delete(&pods_api, &server_pod_name).await;
			pods::delete(&pods_api, &client_pod_name).await;
			emit_attempt_metric_loopback(&rack, "fail");
			patch_loopback(
				&nodes_api,
				&node.name,
				&Verdict::Fail(&format!("watch client pod: {e}")),
			)
			.await;
			return;
		}
	};

	let verdict = match phase {
		pods::Phase::Succeeded => match pods::read_record(&pods_api, &client_pod_name).await {
			Ok(r) if r.ok => {
				emit_attempt_metric_loopback(&rack, "ok");
				patch_loopback(&nodes_api, &node.name, &Verdict::Ok(&r)).await;
				return cleanup(&pods_api, &server_pod_name, &client_pod_name).await;
			}
			Ok(r) => Verdict::Fail(&r.message.clone().unwrap_or_else(|| "probe ok=false".into())).to_owned_msg(),
			Err(e) => OwnedVerdict::Fail(format!("read client pod: {e}")),
		},
		pods::Phase::Failed => OwnedVerdict::Fail("client pod phase Failed".into()),
		pods::Phase::Timeout => OwnedVerdict::Timeout,
	};
	emit_attempt_metric_loopback(&rack, verdict.label());
	patch_loopback(&nodes_api, &node.name, &verdict.borrow()).await;
	cleanup(&pods_api, &server_pod_name, &client_pod_name).await;
}

fn emit_attempt_metric_loopback(rack: &str, verdict: &str) {
	let meter = global::meter("accelrd.prober");
	let counter = meter.u64_counter("roce.probe.loopback.attempts.total").build();
	counter.add(
		1,
		&[
			KeyValue::new("rack", rack.to_string()),
			KeyValue::new("verdict", verdict.to_string()),
		],
	);
}

/// Owned variant for verdicts whose message is computed in this scope —
/// `Verdict<'a>::Fail(&'a str)` requires a borrow that doesn't survive
/// across the await boundary.
enum OwnedVerdict {
	Fail(String),
	Timeout,
}

impl OwnedVerdict {
	fn label(&self) -> &'static str {
		match self {
			Self::Fail(_) => "fail",
			Self::Timeout => "timeout",
		}
	}
	fn borrow(&self) -> Verdict<'_> {
		match self {
			Self::Fail(s) => Verdict::Fail(s.as_str()),
			Self::Timeout => Verdict::Timeout,
		}
	}
}

impl<'a> Verdict<'a> {
	fn to_owned_msg(&self) -> OwnedVerdict {
		match self {
			Self::Fail(s) => OwnedVerdict::Fail((*s).into()),
			Self::Timeout => OwnedVerdict::Timeout,
			Self::Ok(_) => OwnedVerdict::Fail("ok-converted-to-fail".into()),
		}
	}
}

fn pair_tag(a: &str, b: &str) -> String {
	use std::hash::{Hash, Hasher};
	let mut h = std::collections::hash_map::DefaultHasher::new();
	let pair: [&str; 2] = if a < b { [a, b] } else { [b, a] };
	pair.hash(&mut h);
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs()
		.hash(&mut h);
	format!("{:016x}", h.finish())
}

fn emit_cycle_metric(elapsed: Duration) {
	let meter = global::meter("accelrd.prober");
	let h = meter
		.f64_histogram("roce.probe.cycle.duration.ms")
		.with_unit("ms")
		.build();
	h.record(elapsed.as_secs_f64() * 1000.0, &[]);
}

fn emit_attempt_metric(rack: &str, verdict: &str) {
	let meter = global::meter("accelrd.prober");
	let counter = meter.u64_counter("roce.probe.attempts.total").build();
	counter.add(
		1,
		&[
			KeyValue::new("rack", rack.to_string()),
			KeyValue::new("verdict", verdict.to_string()),
		],
	);
}
