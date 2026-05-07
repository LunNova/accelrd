// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Prober subcommand: a kube-rs `Controller` that watches Node objects,
//! groups them by `accel-topo.lunnova.dev/rack`, and for each group of
//! ≥2 same-rack nodes periodically schedules a 2-pod RoCE bandwidth
//! probe. Probe pods run `accelrd probe` (server + client modes) and
//! report results via OTLP + Node annotations written by the prober.
//!
//! Cluster-scoped resource model: prober watches Nodes (cluster-wide)
//! and creates ephemeral Pod objects in its own namespace. It does NOT
//! delegate to Kueue/Volcano — pair selection happens here, and probe
//! pods are pinned to specific nodes via `nodeName` so gang-scheduling
//! reduces to "the scheduler is told which nodes to bind to."

use argh::FromArgs;

use crate::config::{env_string, env_u64};

/// Cluster-scoped controller: pairs same-rack nodes and creates RoCE probe pods.
#[derive(FromArgs, Debug, Clone)]
#[argh(subcommand, name = "prober")]
pub struct ProberArgs {
	/// OTLP endpoint for the prober's own telemetry — separate from the
	/// probe pods' telemetry, which they emit themselves
	/// (env: ACCELRD_OTLP_ENDPOINT; default: http://127.0.0.1:4318)
	#[argh(option)]
	pub otlp_endpoint: Option<String>,

	/// override the OTel `service.name` resource attribute
	/// (env: OTEL_SERVICE_NAME; default: accelrd-prober)
	#[argh(option)]
	pub service_name: Option<String>,

	/// minimum interval between RoCE probes for any given (rack, pair).
	/// Probes consume RoCE bandwidth on the nodes under test, keep it sparse.
	/// (env: ACCELRD_PROBER_CADENCE_SECS; default: 21600 [6h])
	#[argh(option)]
	pub cadence_secs: Option<u64>,

	/// image to use for probe pods. When unset, the prober tries to read
	/// its own image from the K8s downward API; otherwise expects a value here
	/// (env: ACCELRD_PROBER_PROBE_IMAGE)
	#[argh(option)]
	pub probe_image: Option<String>,

	/// namespace where probe pods are created
	/// (env: ACCELRD_PROBER_NAMESPACE; default: accelrd)
	#[argh(option)]
	pub namespace: Option<String>,

	/// per-probe wall-clock test duration in seconds
	/// (env: ACCELRD_PROBER_TEST_DURATION_SECS; default: 30)
	#[argh(option)]
	pub test_duration_secs: Option<u64>,

	/// run one reconcile pass and exit (used in tests)
	#[argh(switch)]
	pub once: bool,
}

#[derive(Debug, Clone)]
pub struct Resolved {
	pub otlp_endpoint: String,
	pub service_name: String,
	pub cadence_secs: u64,
	pub probe_image: Option<String>,
	pub namespace: String,
	pub test_duration_secs: u64,
	pub once: bool,
}

impl ProberArgs {
	pub fn resolve(self) -> Resolved {
		Resolved {
			otlp_endpoint: self
				.otlp_endpoint
				.or_else(|| env_string("ACCELRD_OTLP_ENDPOINT"))
				.unwrap_or_else(|| "http://127.0.0.1:4318".into()),
			service_name: self
				.service_name
				.or_else(|| env_string("OTEL_SERVICE_NAME"))
				.unwrap_or_else(|| "accelrd-prober".into()),
			cadence_secs: self.cadence_secs.or_else(|| env_u64("ACCELRD_PROBER_CADENCE_SECS")).unwrap_or(21_600),
			probe_image: self.probe_image.or_else(|| env_string("ACCELRD_PROBER_PROBE_IMAGE")),
			namespace: self
				.namespace
				.or_else(|| env_string("ACCELRD_PROBER_NAMESPACE"))
				.unwrap_or_else(|| "accelrd".into()),
			test_duration_secs: self
				.test_duration_secs
				.or_else(|| env_u64("ACCELRD_PROBER_TEST_DURATION_SECS"))
				.unwrap_or(30),
			once: self.once,
		}
	}
}

pub async fn run(_args: Resolved) -> anyhow::Result<()> {
	// TODO: implement the controller. Sketch of the reconcile loop:
	//
	// 1. List Nodes. Group by `accel-topo.lunnova.dev/rack` value, drop
	//    racks with <2 healthy nodes, drop racks tested within `cadence_secs`.
	// 2. For each remaining rack, deterministically pick two nodes (sort
	//    by name, rotate by `(epoch_hour / cadence_hours) mod len`).
	// 3. Create two Pods: server on node[0], client on node[1]. Both
	//    invoke `accelrd probe`. Server pod's IP is passed to the client
	//    via env var (set in the client pod spec at create time).
	// 4. Watch the Pods until both terminate. Parse the client pod's
	//    stdout for the probe verdict, patch both nodes'
	//    accel-test.lunnova.dev/last-rack-{at,bandwidth-gbps,partner,verdict}
	//    annotations, delete the Pods.
	// 5. If something goes wrong (timeout, pod scheduling failure),
	//    write `verdict=fail` annotations with a reason and move on.
	tracing::warn!("prober subcommand not yet implemented");
	Ok(())
}
