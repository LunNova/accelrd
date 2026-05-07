// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Probe-pod construction. We synthesize Pod specs as `serde_json::Value`
//! and let kube-rs deserialize → `Pod`; cleaner than instantiating the
//! deeply-nested k8s_openapi types positionally.
//!
//! Probe pods run with hostNetwork (so the server's IP equals the node's
//! InternalIP, no Service needed), root-in-container (so CAP_NET_RAW +
//! CAP_IPC_LOCK actually land in the effective set on exec), and a
//! hostPath mount on `/dev/infiniband` so libibverbs can open the
//! kernel uverbs files. RDMA hardware access without a device plugin
//! requires either privileged or these specific capabilities.

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams, LogParams, PostParams};
use serde_json::{Value, json};

use crate::prober::Resolved;
use crate::prober::results::ProbeRecord;

pub const APP_LABEL: &str = "accelrd-probe";
pub const ROLE_LABEL: &str = "accel-test.lunnova.dev/role";
pub const RACK_LABEL: &str = "accel-test.lunnova.dev/rack";
pub const PAIR_LABEL: &str = "accel-test.lunnova.dev/pair";
pub const APP_LABEL_KEY: &str = "app.kubernetes.io/name";

#[derive(Debug, Clone, Copy)]
pub enum Role {
	Server,
	Client,
}

impl Role {
	pub fn slug(&self) -> &'static str {
		match self {
			Self::Server => "server",
			Self::Client => "client",
		}
	}
}

pub struct PodSpec<'a> {
	pub role: Role,
	pub node_name: &'a str,
	pub partner_node: &'a str,
	pub partner_ip: Option<&'a str>,
	pub rack: &'a str,
	pub pair_id: &'a str,
	pub args: &'a Resolved,
}

pub fn build_pod(s: &PodSpec<'_>) -> anyhow::Result<Pod> {
	let image = s
		.args
		.probe_image
		.as_deref()
		.context("probe_image (set ACCELRD_PROBER_PROBE_IMAGE)")?;
	let role = s.role.slug();
	let mut env: Vec<Value> = vec![
		json!({"name": "ACCELRD_PROBE_DURATION_SECS", "value": s.args.test_duration_secs.to_string()}),
		json!({"name": "ACCELRD_PROBE_SRC_RACK", "value": s.rack}),
		json!({"name": "ACCELRD_PROBE_PARTNER_NODE", "value": s.partner_node}),
		json!({"name": "ACCELRD_OTLP_ENDPOINT", "value": s.args.otlp_endpoint}),
		json!({"name": "OTEL_SERVICE_NAME", "value": "accelrd-probe"}),
		json!({"name": "RUST_LOG", "value": "info,opentelemetry=warn,reqwest=error"}),
		json!({"name": "NODE_NAME", "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}}),
		json!({"name": "POD_NAME", "valueFrom": {"fieldRef": {"fieldPath": "metadata.name"}}}),
		json!({"name": "POD_UID", "valueFrom": {"fieldRef": {"fieldPath": "metadata.uid"}}}),
		json!({"name": "POD_NAMESPACE", "valueFrom": {"fieldRef": {"fieldPath": "metadata.namespace"}}}),
		json!({"name": "CONTAINER_NAME", "value": "probe"}),
	];
	if let Role::Client = s.role
		&& let Some(ip) = s.partner_ip
	{
		env.push(json!({"name": "ACCELRD_PROBE_SERVER", "value": ip}));
	}

	let pod = json!({
		"apiVersion": "v1",
		"kind": "Pod",
		"metadata": {
			"generateName": format!("accelrd-probe-{role}-"),
			"namespace": s.args.namespace,
			"labels": {
				APP_LABEL_KEY: APP_LABEL,
				"app.kubernetes.io/component": "probe",
				"app.kubernetes.io/managed-by": "accelrd-prober",
				ROLE_LABEL: role,
				RACK_LABEL: s.rack,
				PAIR_LABEL: s.pair_id,
			},
		},
		"spec": {
			"restartPolicy": "Never",
			"hostNetwork": true,
			"dnsPolicy": "ClusterFirstWithHostNet",
			"nodeName": s.node_name,
			"automountServiceAccountToken": false,
			"tolerations": [
				{"key": "nvidia.com/gpu", "operator": "Exists", "effect": "NoSchedule"},
				{"key": "amd.com/gpu", "operator": "Exists", "effect": "NoSchedule"},
				{"key": "accelerator", "operator": "Exists", "effect": "NoSchedule"},
			],
			"containers": [{
				"name": "probe",
				"image": image,
				"imagePullPolicy": "IfNotPresent",
				"args": ["probe", "--mode", role],
				"env": env,
				"securityContext": {
					"runAsUser": 0,
					"allowPrivilegeEscalation": false,
					"readOnlyRootFilesystem": true,
					"capabilities": {
						// IPC_LOCK lets ib_send_bw mlock the data buffers
						// (RDMA requires pinned pages); NET_RAW lets the
						// kernel ibverbs path access the rdma_cm socket.
						"drop": ["ALL"],
						"add": ["IPC_LOCK", "NET_RAW"],
					},
					"seccompProfile": {"type": "RuntimeDefault"},
				},
				"resources": {
					"requests": {"cpu": "100m", "memory": "64Mi"},
					"limits": {"cpu": "500m", "memory": "256Mi"},
				},
				"volumeMounts": [
					{"name": "dev-infiniband", "mountPath": "/dev/infiniband"},
					{"name": "sys", "mountPath": "/sys", "readOnly": true},
				],
			}],
			"volumes": [
				{"name": "dev-infiniband", "hostPath": {"path": "/dev/infiniband"}},
				{"name": "sys", "hostPath": {"path": "/sys", "type": "Directory"}},
			],
		},
	});
	serde_json::from_value(pod).context("synthesize Pod from JSON template")
}

pub async fn create(api: &Api<Pod>, pod: &Pod) -> anyhow::Result<Pod> {
	api.create(&PostParams::default(), pod)
		.await
		.context("create probe pod")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
	Succeeded,
	Failed,
	Timeout,
}

pub async fn await_terminal(api: &Api<Pod>, name: &str, timeout: Duration) -> anyhow::Result<Phase> {
	let deadline = Instant::now() + timeout;
	let mut interval = tokio::time::interval(Duration::from_secs(2));
	loop {
		interval.tick().await;
		if Instant::now() >= deadline {
			return Ok(Phase::Timeout);
		}
		match api.get(name).await {
			Ok(p) => {
				let phase = p.status.as_ref().and_then(|s| s.phase.as_deref()).unwrap_or("");
				match phase {
					"Succeeded" => return Ok(Phase::Succeeded),
					"Failed" => return Ok(Phase::Failed),
					_ => {}
				}
			}
			Err(kube::Error::Api(e)) if e.code == 404 => {
				// Pod was deleted out from under us. Treat as failure.
				return Err(anyhow!("pod {name} disappeared during probe"));
			}
			Err(e) => {
				tracing::debug!(error = %e, pod = name, "transient pod-get error");
			}
		}
	}
}

pub async fn read_record(api: &Api<Pod>, name: &str) -> anyhow::Result<ProbeRecord> {
	let log = api
		.logs(
			name,
			&LogParams {
				tail_lines: Some(64),
				..Default::default()
			},
		)
		.await
		.context("read probe logs")?;
	crate::prober::results::parse(&log)
}

pub async fn delete(api: &Api<Pod>, name: &str) {
	if let Err(e) = api.delete(name, &DeleteParams::default()).await {
		tracing::warn!(error = %e, pod = name, "probe pod delete failed (will retry next cycle)");
	}
}

/// Best-effort orphan cleanup at the start of each reconcile cycle:
/// any pod with `app.kubernetes.io/name=accelrd-probe` older than
/// `max_age` whose owner prober crashed mid-probe (or is finishing
/// teardown) gets deleted.
pub async fn cleanup_orphans(api: &Api<Pod>, max_age: Duration) {
	let lp = ListParams::default().labels(&format!("{APP_LABEL_KEY}={APP_LABEL}"));
	let list = match api.list(&lp).await {
		Ok(l) => l,
		Err(e) => {
			tracing::debug!(error = %e, "orphan cleanup list failed");
			return;
		}
	};
	let cutoff_rfc3339 = crate::time::rfc3339_at(
		(std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or_default()
			.as_secs()
			- max_age.as_secs()) as i64,
	);
	for pod in list {
		let Some(name) = pod.metadata.name.as_deref() else {
			continue;
		};
		let created = pod
			.metadata
			.creation_timestamp
			.as_ref()
			.map(|t| crate::time::rfc3339_at(t.0.as_second()));
		let stale = match created {
			Some(c) => c < cutoff_rfc3339,
			None => false,
		};
		if stale {
			tracing::info!(pod = name, "deleting orphaned probe pod");
			delete(api, name).await;
		}
	}
}
