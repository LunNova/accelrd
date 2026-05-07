// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Admin subcommand: read-only cluster console.
//!
//! Runs as a singleton Deployment in the `accelrd` namespace. Reads
//! cluster topology and probe results from the K8s API (the labels and
//! annotations the daemon and prober write), and pulls live accelerator
//! metrics from a `mutel` instance the OTLP pipeline already feeds.
//! Serves a small static SPA (HTML/CSS/JS embedded at compile time) and
//! a JSON API at `/api/*`. ClusterIP-only by design — operators reach
//! it via `kubectl port-forward`, no auth.

mod assets;
mod k8s;
mod mutel;
mod routes;
mod state;

use std::net::SocketAddr;

use anyhow::Context;
use argh::FromArgs;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{env_string, env_u64};
use crate::exporter;

/// Read-only cluster admin console (web UI).
#[derive(FromArgs, Debug, Clone)]
#[argh(subcommand, name = "admin")]
pub struct AdminArgs {
	/// listen address for the admin HTTP server
	/// (env: ACCELRD_ADMIN_LISTEN; default: 0.0.0.0:8080)
	#[argh(option)]
	pub listen: Option<String>,

	/// base URL of a mutel instance (the OTLP backend the daemons ship to)
	/// for live metric/log queries. The admin UI surfaces a banner when
	/// unreachable rather than failing the page.
	/// (env: ACCELRD_MUTEL_ENDPOINT; default: http://mutel.observability.svc.cluster.local:4318)
	#[argh(option)]
	pub mutel_endpoint: Option<String>,

	/// OTLP endpoint for the admin's *own* self-telemetry — usually the
	/// same mutel instance, but kept as a separate flag so it can point
	/// elsewhere if needed
	/// (env: ACCELRD_OTLP_ENDPOINT; default: http://127.0.0.1:4318)
	#[argh(option)]
	pub otlp_endpoint: Option<String>,

	/// override the OTel `service.name` resource attribute
	/// (env: OTEL_SERVICE_NAME; default: accelrd-admin)
	#[argh(option)]
	pub service_name: Option<String>,

	/// HTTP timeout for mutel queries, seconds
	/// (env: ACCELRD_ADMIN_MUTEL_TIMEOUT_SECS; default: 5)
	#[argh(option)]
	pub mutel_timeout_secs: Option<u64>,

	/// skip K8s client init even if a SA token is mounted (useful for
	/// running the UI locally without cluster access — all `/api/*`
	/// cluster endpoints return empty stubs)
	#[argh(switch)]
	pub no_k8s: bool,
}

#[derive(Debug, Clone)]
pub struct Resolved {
	pub listen: SocketAddr,
	pub mutel_endpoint: String,
	pub otlp_endpoint: String,
	pub service_name: String,
	pub mutel_timeout_secs: u64,
	pub no_k8s: bool,
}

impl AdminArgs {
	pub fn resolve(self) -> anyhow::Result<Resolved> {
		let listen_str = self
			.listen
			.or_else(|| env_string("ACCELRD_ADMIN_LISTEN"))
			.unwrap_or_else(|| "0.0.0.0:8080".into());
		let listen: SocketAddr = listen_str
			.parse()
			.with_context(|| format!("parse listen addr {listen_str:?}"))?;
		Ok(Resolved {
			listen,
			mutel_endpoint: self
				.mutel_endpoint
				.or_else(|| env_string("ACCELRD_MUTEL_ENDPOINT"))
				.unwrap_or_else(|| "http://mutel.observability.svc.cluster.local:4318".into()),
			otlp_endpoint: self
				.otlp_endpoint
				.or_else(|| env_string("ACCELRD_OTLP_ENDPOINT"))
				.unwrap_or_else(|| "http://127.0.0.1:4318".into()),
			service_name: self
				.service_name
				.or_else(|| env_string("OTEL_SERVICE_NAME"))
				.unwrap_or_else(|| "accelrd-admin".into()),
			mutel_timeout_secs: self
				.mutel_timeout_secs
				.or_else(|| env_u64("ACCELRD_ADMIN_MUTEL_TIMEOUT_SECS"))
				.unwrap_or(5),
			no_k8s: self.no_k8s,
		})
	}
}

pub async fn run(args: Resolved) -> anyhow::Result<()> {
	let node_name = host_identity();
	let providers = exporter::init(&args.otlp_endpoint, &args.service_name, &node_name)?;
	init_tracing(&providers);

	let state = state::AppState::build(&args).await?;
	let app = routes::router(state);

	tracing::info!(
		listen = %args.listen,
		mutel = %args.mutel_endpoint,
		k8s = !args.no_k8s,
		"accelrd admin console starting",
	);

	let listener = tokio::net::TcpListener::bind(args.listen)
		.await
		.with_context(|| format!("bind {}", args.listen))?;
	let serve = axum::serve(listener, app).with_graceful_shutdown(async {
		let _ = tokio::signal::ctrl_c().await;
		tracing::info!("shutdown requested");
	});
	let result = serve.await.context("axum serve");

	providers.shutdown();
	result
}

fn host_identity() -> String {
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

/// Mirror `lifecycle::init_tracing` — fmt + OTel span/log layers — so
/// the admin's own logs and spans flow through the same OTLP pipeline.
fn init_tracing(providers: &exporter::Providers) {
	use opentelemetry::trace::TracerProvider as _;

	let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
	let otel_filter = || {
		tracing_subscriber::filter::Targets::new()
			.with_target("reqwest", tracing::Level::ERROR)
			.with_target("hyper_util", tracing::Level::ERROR)
			.with_target("opentelemetry", tracing::Level::WARN)
			.with_target("tower_http", tracing::Level::WARN)
			.with_default(tracing::Level::INFO)
	};
	let otel_tracer = providers.tracer.tracer("accelrd-admin");
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
