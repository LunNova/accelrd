// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Prober subcommand: a long-running controller that pairs same-rack
//! nodes and creates RoCE bandwidth probe Pods. Watches Nodes
//! cluster-wide; creates ephemeral Pods (server + client) in its own
//! namespace; reports results via OTLP and node annotations under
//! `accel-test.lunnova.dev/last-rack-*`.
//!
//! Cluster-scoped resource model: probe pods are pinned to specific
//! nodes via `nodeName`, so gang-scheduling reduces to "the scheduler
//! is told which nodes to bind to."

mod annotations;
mod pair;
mod pods;
mod reconcile;
mod results;

use argh::FromArgs;
use kube::Client;
use opentelemetry::trace::TracerProvider;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{env_string, env_u64};
use crate::exporter;

pub use annotations::ANN_LAST_AT;

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

	/// reconcile loop interval — how often we wake up to look for racks
	/// past their cadence cutoff. Should be ≪ cadence_secs.
	/// (env: ACCELRD_PROBER_RECONCILE_INTERVAL_SECS; default: 300 [5min])
	#[argh(option)]
	pub reconcile_interval_secs: Option<u64>,

	/// image to use for probe pods. Required — we can't reliably read it
	/// from the prober's own pod via downward API
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

	/// max number of concurrent probe pairs across the cluster
	/// (env: ACCELRD_PROBER_MAX_CONCURRENT_PAIRS; default: 4)
	#[argh(option)]
	pub max_concurrent_pairs: Option<u32>,

	/// run one reconcile pass and exit (used in tests)
	#[argh(switch)]
	pub once: bool,
}

#[derive(Debug, Clone)]
pub struct Resolved {
	pub otlp_endpoint: String,
	pub service_name: String,
	pub cadence_secs: u64,
	pub reconcile_interval_secs: u64,
	pub probe_image: Option<String>,
	pub namespace: String,
	pub test_duration_secs: u64,
	pub max_concurrent_pairs: u32,
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
			cadence_secs: self
				.cadence_secs
				.or_else(|| env_u64("ACCELRD_PROBER_CADENCE_SECS"))
				.unwrap_or(21_600),
			reconcile_interval_secs: self
				.reconcile_interval_secs
				.or_else(|| env_u64("ACCELRD_PROBER_RECONCILE_INTERVAL_SECS"))
				.unwrap_or(300),
			probe_image: self.probe_image.or_else(|| env_string("ACCELRD_PROBER_PROBE_IMAGE")),
			namespace: self
				.namespace
				.or_else(|| env_string("ACCELRD_PROBER_NAMESPACE"))
				.unwrap_or_else(|| "accelrd".into()),
			test_duration_secs: self
				.test_duration_secs
				.or_else(|| env_u64("ACCELRD_PROBER_TEST_DURATION_SECS"))
				.unwrap_or(30),
			max_concurrent_pairs: self
				.max_concurrent_pairs
				.or_else(|| env_u64("ACCELRD_PROBER_MAX_CONCURRENT_PAIRS").map(|v| v as u32))
				.unwrap_or(4),
			once: self.once,
		}
	}
}

pub async fn run(args: Resolved) -> anyhow::Result<()> {
	let providers = exporter::init(&args.otlp_endpoint, &args.service_name, &node_name())?;
	init_tracing(&providers);

	if args.probe_image.is_none() {
		anyhow::bail!("probe_image is required (set --probe-image or ACCELRD_PROBER_PROBE_IMAGE)");
	}

	let client = Client::try_default().await?;
	tracing::info!(
		namespace = %args.namespace,
		cadence_secs = args.cadence_secs,
		reconcile_interval_secs = args.reconcile_interval_secs,
		max_concurrent_pairs = args.max_concurrent_pairs,
		"prober starting",
	);

	if args.once {
		reconcile::run_once(&client, &args).await;
		providers.shutdown();
		return Ok(());
	}

	let mut interval = tokio::time::interval(std::time::Duration::from_secs(args.reconcile_interval_secs));
	loop {
		tokio::select! {
			_ = interval.tick() => {
				let span = tracing::info_span!("reconcile_cycle");
				let _enter = span.enter();
				reconcile::run_once(&client, &args).await;
			}
			_ = tokio::signal::ctrl_c() => {
				tracing::info!("shutdown requested");
				break;
			}
		}
	}
	providers.shutdown();
	Ok(())
}

fn node_name() -> String {
	std::env::var("NODE_NAME")
		.ok()
		.filter(|s| !s.is_empty())
		.or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
		.or_else(|| {
			std::fs::read_to_string("/proc/sys/kernel/hostname")
				.ok()
				.map(|s| s.trim().to_string())
		})
		.filter(|s| !s.is_empty())
		.unwrap_or_else(|| "unknown".into())
}

fn init_tracing(providers: &exporter::Providers) {
	let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
	let otel_filter = || {
		tracing_subscriber::filter::Targets::new()
			.with_target("reqwest", tracing::Level::ERROR)
			.with_target("hyper_util", tracing::Level::ERROR)
			.with_target("opentelemetry", tracing::Level::WARN)
			.with_default(tracing::Level::INFO)
	};
	let otel_tracer = providers.tracer.tracer("accelrd-prober");
	let span_layer = tracing_opentelemetry::layer()
		.with_tracer(otel_tracer)
		.with_filter(otel_filter());
	let log_layer = OpenTelemetryTracingBridge::new(&providers.logger).with_filter(otel_filter());
	tracing_subscriber::registry()
		.with(env_filter)
		.with(tracing_subscriber::fmt::layer())
		.with(span_layer)
		.with(log_layer)
		.init();
}
