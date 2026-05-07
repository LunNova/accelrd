// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Wire everything together. Init exporters, enumerate, build labels,
//! spawn live + preflight loops, wait for SIGINT.

use std::time::Duration;

use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::Args;
use crate::k8s::{self, facts, labeler::OptionalLabeler};
use crate::preflight::default_registry;
use crate::sensors::enumerate_all;
use crate::telemetry::{live, preflight as pf};
use crate::topology::{discover, labels as topo_labels};
use crate::{exporter, telemetry::preflight::PreflightInputs};

pub async fn run(args: Args) -> anyhow::Result<()> {
	let node_name = facts::node_name(&args);
	let providers = exporter::init(&args, &node_name)?;
	init_tracing(&providers);

	let (mut accelerators, backends) = enumerate_all().await?;
	let topology = discover::discover(&args, &mut accelerators);

	for a in &accelerators {
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
		"topology",
	);

	let labels = topo_labels::build(&topology, &accelerators);
	let labeler = OptionalLabeler::new(k8s::enabled(&args), node_name.clone()).await;

	let live_interval = Duration::from_secs(args.live_interval_secs);
	let preflight_interval = Duration::from_secs(args.preflight_interval_secs);

	if args.once {
		// Run one of each, sequentially, for deterministic test mode.
		live::run(backends, accelerators.clone(), live_interval, true).await;
		pf::run(
			PreflightInputs {
				checks: default_registry(),
				accelerators,
				topology,
				interval: preflight_interval,
				once: true,
				label_base: labels,
			},
			labeler,
		)
		.await;
	} else {
		let live_handle = tokio::spawn(live::run(backends, accelerators.clone(), live_interval, false));
		let pf_handle = tokio::spawn(pf::run(
			PreflightInputs {
				checks: default_registry(),
				accelerators,
				topology,
				interval: preflight_interval,
				once: false,
				label_base: labels,
			},
			labeler,
		));
		let _ = tokio::signal::ctrl_c().await;
		tracing::info!("shutdown requested");
		live_handle.abort();
		pf_handle.abort();
	}

	providers.shutdown();
	Ok(())
}

/// Tracing-subscriber init. We register two layers:
///  - `fmt` for human-readable stdout (EnvFilter respects RUST_LOG).
///  - `OpenTelemetryTracingBridge` so the same `tracing::info!` calls
///    are also exported as OTLP log records.
/// The bridge filter excludes spammy crates (reqwest, opentelemetry's
/// own internal logs) to avoid telemetry-induced-telemetry loops.
fn init_tracing(providers: &exporter::Providers) {
	let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
	let bridge = OpenTelemetryTracingBridge::new(&providers.logger);
	let bridge_filter = tracing_subscriber::filter::Targets::new()
		.with_target("reqwest", tracing::Level::ERROR)
		.with_target("hyper_util", tracing::Level::ERROR)
		.with_target("opentelemetry", tracing::Level::WARN)
		.with_default(tracing::Level::INFO);
	tracing_subscriber::registry()
		.with(env_filter)
		.with(tracing_subscriber::fmt::layer())
		.with(bridge.with_filter(bridge_filter))
		.init();
}
