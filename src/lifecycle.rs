// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Wire everything together. Init exporters, enumerate, build labels,
//! spawn live + preflight loops, wait for SIGINT.

use std::time::Duration;

use opentelemetry::trace::TracerProvider;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::Args;
use crate::exporter;
use crate::k8s::{self, facts, labeler::OptionalLabeler};
use crate::preflight::default_registry;
use crate::sensors::enumerate_all;
use crate::telemetry::live;
use crate::telemetry::preflight::{self as pf, PreflightInputs};
use crate::topology::{discover, labels as topo_labels};

pub async fn run(args: Args) -> anyhow::Result<()> {
	let node_name = facts::node_name(&args);
	let providers = exporter::init(&args, &node_name)?;
	init_tracing(&providers);

	let (mut accelerators, backends) = enumerate_all().await?;
	let topology = discover::discover(&args, &mut accelerators).await;
	log_inventory(&accelerators, &topology);

	let labels = topo_labels::build(&topology, &accelerators);
	let labeler = OptionalLabeler::new(k8s::enabled(&args), node_name.clone()).await;

	let live_interval = Duration::from_secs(args.live_interval_secs);
	let preflight_inputs = PreflightInputs {
		checks: default_registry(),
		accelerators: accelerators.clone(),
		topology,
		interval: Duration::from_secs(args.preflight_interval_secs),
		once: args.once,
		label_base: labels,
	};
	let live_fut = live::run(backends, accelerators, live_interval, args.once);
	let pf_fut = pf::run(preflight_inputs, labeler);

	if args.once {
		// Sequential, deterministic test mode.
		live_fut.await;
		pf_fut.await;
	} else {
		let live_handle = tokio::spawn(live_fut);
		let pf_handle = tokio::spawn(pf_fut);
		let _ = tokio::signal::ctrl_c().await;
		tracing::info!("shutdown requested");
		live_handle.abort();
		pf_handle.abort();
	}

	providers.shutdown();
	Ok(())
}

fn log_inventory(accelerators: &[crate::sensors::Accelerator], topology: &crate::topology::NodeTopology) {
	for a in accelerators {
		tracing::info!(
			id = %a.id,
			model = %a.model,
			memory_kind = a.memory_kind.slug(),
			memory_total_bytes = ?a.memory_total_bytes,
			coverage = a.coverage.slug(),
			fabric_domain = ?a.fabric_domain,
			numa_node = ?a.numa_node,
			partitioned = a.partitioned,
			"accelerator",
		);
	}
	tracing::info!(
		fabric_domains = topology.fabric_domains.len(),
		region = ?topology.region,
		zone = ?topology.zone,
		block = ?topology.block,
		rack = ?topology.rack,
		lldp_neighbors = topology.lldp_neighbors.len(),
		"topology",
	);
	for n in &topology.lldp_neighbors {
		tracing::info!(
			interface = %n.interface,
			chassis = %n.chassis_id,
			port = %n.port_id,
			system_name = ?n.system_name,
			"lldp neighbor",
		);
	}
}

/// Tracing-subscriber init. Three layers:
///  - `fmt` for human-readable stdout (EnvFilter respects RUST_LOG).
///  - `OpenTelemetryLayer` so `tracing::info_span!` calls become OTLP
///    spans with proper parent/child relationships derived from
///    tracing's span tree (no manual context plumbing required).
///  - `OpenTelemetryTracingBridge` so `tracing::info!` events become
///    OTLP log records.
///
/// The OTel-bound layers exclude spammy crates (reqwest, hyper_util,
/// opentelemetry's own internal logs) to avoid telemetry-induced
/// telemetry loops.
fn init_tracing(providers: &exporter::Providers) {
	let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

	let otel_filter = || {
		tracing_subscriber::filter::Targets::new()
			.with_target("reqwest", tracing::Level::ERROR)
			.with_target("hyper_util", tracing::Level::ERROR)
			.with_target("opentelemetry", tracing::Level::WARN)
			.with_default(tracing::Level::INFO)
	};

	let otel_tracer = providers.tracer.tracer("accelrd");
	let span_layer = tracing_opentelemetry::layer().with_tracer(otel_tracer).with_filter(otel_filter());
	let log_layer = OpenTelemetryTracingBridge::new(&providers.logger).with_filter(otel_filter());

	tracing_subscriber::registry()
		.with(env_filter)
		.with(tracing_subscriber::fmt::layer())
		.with(span_layer)
		.with(log_layer)
		.init();
}
