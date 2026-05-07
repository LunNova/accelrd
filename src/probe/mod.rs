// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Probe subcommand: one side of a paired RoCE bandwidth test.
//!
//! Server mode runs `ib_send_bw -F` and waits for a client. Client mode
//! polls the server until it accepts a connection, then runs the same
//! command pointed at the server, parses the perftest output to extract
//! peak/average bandwidth, emits the result as an OTLP metric, and
//! prints a single JSON object on stdout for the prober to consume.

mod device;
mod net;
mod perftest;

use std::process::Stdio;
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::Context;
use argh::FromArgs;
use opentelemetry::trace::TracerProvider;
use opentelemetry::{KeyValue, global};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use serde::Serialize;
use tokio::process::Command;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::{env_string, env_u64};
use crate::exporter;

/// QP-exchange port; matches `ib_send_bw -p` default.
const PERFTEST_PORT: u16 = 18515;

/// One side of a paired RoCE probe (server or client). Created by the prober.
#[derive(FromArgs, Debug, Clone)]
#[argh(subcommand, name = "probe")]
pub struct ProbeArgs {
	/// which side of the probe pair to run: server or client
	#[argh(option)]
	pub mode: ProbeMode,

	/// server hostname or IP — required in client mode (env: ACCELRD_PROBE_SERVER)
	#[argh(option)]
	pub server: Option<String>,

	/// RDMA device name (e.g. mlx5_0); when unset, picks the first
	/// /sys/class/infiniband entry (env: ACCELRD_PROBE_DEVICE)
	#[argh(option)]
	pub device: Option<String>,

	/// test duration — wrapper-enforced upper bound, perftest controls iters
	/// (env: ACCELRD_PROBE_DURATION_SECS; default: 30)
	#[argh(option)]
	pub duration_secs: Option<u64>,

	/// connect timeout for client mode, seconds — how long the client
	/// will wait for the server's TCP listener to come up
	/// (env: ACCELRD_PROBE_CONNECT_TIMEOUT_SECS; default: 60)
	#[argh(option)]
	pub connect_timeout_secs: Option<u64>,

	/// OTLP endpoint to emit probe results to
	/// (env: ACCELRD_OTLP_ENDPOINT; default: http://127.0.0.1:4318)
	#[argh(option)]
	pub otlp_endpoint: Option<String>,

	/// override the OTel `service.name` resource attribute
	/// (env: OTEL_SERVICE_NAME; default: accelrd-probe)
	#[argh(option)]
	pub service_name: Option<String>,

	/// source-side rack ID (where this pod is running); metric attribute,
	/// set by the prober at pod-create time (env: ACCELRD_PROBE_SRC_RACK)
	#[argh(option)]
	pub src_rack: Option<String>,

	/// partner-side node name (the other end of the pair); metric attribute
	/// (env: ACCELRD_PROBE_PARTNER_NODE)
	#[argh(option)]
	pub partner_node: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeMode {
	Server,
	Client,
}

impl ProbeMode {
	fn slug(&self) -> &'static str {
		match self {
			Self::Server => "server",
			Self::Client => "client",
		}
	}
}

impl FromStr for ProbeMode {
	type Err = String;
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"server" => Ok(ProbeMode::Server),
			"client" => Ok(ProbeMode::Client),
			other => Err(format!("invalid mode {other:?}; expected 'server' or 'client'")),
		}
	}
}

#[derive(Debug, Clone)]
pub struct Resolved {
	pub mode: ProbeMode,
	pub server: Option<String>,
	pub device: Option<String>,
	pub duration_secs: u64,
	pub connect_timeout_secs: u64,
	pub otlp_endpoint: String,
	pub service_name: String,
	pub src_rack: Option<String>,
	pub partner_node: Option<String>,
}

impl ProbeArgs {
	pub fn resolve(self) -> Resolved {
		Resolved {
			mode: self.mode,
			server: self.server.or_else(|| env_string("ACCELRD_PROBE_SERVER")),
			device: self.device.or_else(|| env_string("ACCELRD_PROBE_DEVICE")),
			duration_secs: self
				.duration_secs
				.or_else(|| env_u64("ACCELRD_PROBE_DURATION_SECS"))
				.unwrap_or(30),
			connect_timeout_secs: self
				.connect_timeout_secs
				.or_else(|| env_u64("ACCELRD_PROBE_CONNECT_TIMEOUT_SECS"))
				.unwrap_or(60),
			otlp_endpoint: self
				.otlp_endpoint
				.or_else(|| env_string("ACCELRD_OTLP_ENDPOINT"))
				.unwrap_or_else(|| "http://127.0.0.1:4318".into()),
			service_name: self
				.service_name
				.or_else(|| env_string("OTEL_SERVICE_NAME"))
				.unwrap_or_else(|| "accelrd-probe".into()),
			src_rack: self.src_rack.or_else(|| env_string("ACCELRD_PROBE_SRC_RACK")),
			partner_node: self.partner_node.or_else(|| env_string("ACCELRD_PROBE_PARTNER_NODE")),
		}
	}
}

/// Single-line JSON record printed on stdout. The prober reads its
/// client pod's stdout and parses this; downstream telemetry/CRD writers
/// also consume the same shape.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
	pub side: &'static str,
	pub ok: bool,
	pub server: Option<String>,
	pub device: String,
	pub bytes: Option<u64>,
	pub bw_peak_gbps: Option<f64>,
	pub bw_avg_gbps: Option<f64>,
	pub msg_rate_mpps: Option<f64>,
	pub duration_s: f64,
	pub message: Option<String>,
	pub src_rack: Option<String>,
	pub partner_node: Option<String>,
}

pub async fn run(args: Resolved) -> anyhow::Result<()> {
	let providers = exporter::init(&args.otlp_endpoint, &args.service_name, &node_name())?;
	init_tracing(&providers);

	let device = device::select(args.device.as_deref()).context("rdma device selection")?;

	tracing::info!(
		side = args.mode.slug(),
		device,
		duration_s = args.duration_secs,
		server = args.server.as_deref().unwrap_or("-"),
		"probe starting",
	);

	let started = Instant::now();
	let outcome = match args.mode {
		ProbeMode::Server => run_server(&device, args.duration_secs).await,
		ProbeMode::Client => {
			let server = args
				.server
				.clone()
				.context("--server (or ACCELRD_PROBE_SERVER) required in client mode")?;
			run_client(&server, &device, args.duration_secs, args.connect_timeout_secs).await
		}
	};
	let elapsed = started.elapsed().as_secs_f64();

	let record = build_record(&args, &device, elapsed, outcome);
	let line = serde_json::to_string(&record).context("serialize probe record")?;
	println!("{line}");
	emit_metric(&record);

	tracing::info!(
		side = record.side,
		ok = record.ok,
		bw_avg_gbps = ?record.bw_avg_gbps,
		bw_peak_gbps = ?record.bw_peak_gbps,
		msg_rate_mpps = ?record.msg_rate_mpps,
		duration_s = record.duration_s,
		"probe complete",
	);

	providers.shutdown();

	if record.ok {
		Ok(())
	} else {
		anyhow::bail!("probe failed: {}", record.message.as_deref().unwrap_or("unknown"))
	}
}

enum Outcome {
	Ok(perftest::Summary),
	Err(String),
}

async fn run_server(device: &str, duration_secs: u64) -> Outcome {
	let std_cmd = perftest::build_cmd(None, device, PERFTEST_PORT, duration_secs);
	let mut cmd = Command::from(std_cmd);
	let output = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output().await {
		Ok(o) => o,
		Err(e) => return Outcome::Err(format!("spawn ib_send_bw: {e}")),
	};
	let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
	let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
	if !output.status.success() {
		return Outcome::Err(format!(
			"ib_send_bw exit {}: {}",
			output.status,
			stderr.trim().lines().last().unwrap_or("")
		));
	}
	match perftest::parse_required(&stdout) {
		Ok(s) => Outcome::Ok(s),
		Err(e) => Outcome::Err(e.to_string()),
	}
}

async fn run_client(server: &str, device: &str, duration_secs: u64, connect_timeout_secs: u64) -> Outcome {
	let deadline = Instant::now() + Duration::from_secs(connect_timeout_secs);
	if let Err(e) = net::wait_connectable(server, PERFTEST_PORT, deadline).await {
		return Outcome::Err(format!("server unreachable: {e}"));
	}
	let std_cmd = perftest::build_cmd(Some(server), device, PERFTEST_PORT, duration_secs);
	let mut cmd = Command::from(std_cmd);
	let output = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output().await {
		Ok(o) => o,
		Err(e) => return Outcome::Err(format!("spawn ib_send_bw: {e}")),
	};
	let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
	let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
	if !output.status.success() {
		return Outcome::Err(format!(
			"ib_send_bw exit {}: {}",
			output.status,
			stderr.trim().lines().last().unwrap_or("")
		));
	}
	match perftest::parse_required(&stdout) {
		Ok(s) => Outcome::Ok(s),
		Err(e) => Outcome::Err(e.to_string()),
	}
}

fn build_record(args: &Resolved, device: &str, duration_s: f64, outcome: Outcome) -> Record {
	let common = Record {
		side: args.mode.slug(),
		ok: false,
		server: args.server.clone(),
		device: device.into(),
		bytes: None,
		bw_peak_gbps: None,
		bw_avg_gbps: None,
		msg_rate_mpps: None,
		duration_s,
		message: None,
		src_rack: args.src_rack.clone(),
		partner_node: args.partner_node.clone(),
	};
	match outcome {
		Outcome::Ok(s) => Record {
			ok: true,
			bytes: Some(s.bytes),
			bw_peak_gbps: Some(s.bw_peak_gbps),
			bw_avg_gbps: Some(s.bw_avg_gbps),
			msg_rate_mpps: Some(s.msg_rate_mpps),
			..common
		},
		Outcome::Err(msg) => Record {
			ok: false,
			message: Some(msg),
			..common
		},
	}
}

fn emit_metric(record: &Record) {
	let meter = global::meter("accelrd.probe");
	let mut attrs = vec![
		KeyValue::new("side", record.side),
		KeyValue::new("device", record.device.clone()),
		KeyValue::new("ok", record.ok),
	];
	if let Some(s) = &record.server {
		attrs.push(KeyValue::new("server", s.clone()));
	}
	if let Some(s) = &record.src_rack {
		attrs.push(KeyValue::new("src_rack", s.clone()));
	}
	if let Some(s) = &record.partner_node {
		attrs.push(KeyValue::new("partner_node", s.clone()));
	}
	let bw_avg = meter
		.f64_gauge("roce.probe.bandwidth.avg.gbps")
		.with_unit("Gb/s")
		.with_description("Average RoCE bandwidth between paired pods, perftest --report_gbits")
		.build();
	bw_avg.record(record.bw_avg_gbps.unwrap_or(0.0), &attrs);
	let bw_peak = meter
		.f64_gauge("roce.probe.bandwidth.peak.gbps")
		.with_unit("Gb/s")
		.with_description("Peak RoCE bandwidth between paired pods, perftest --report_gbits")
		.build();
	bw_peak.record(record.bw_peak_gbps.unwrap_or(0.0), &attrs);
	let mrate = meter
		.f64_gauge("roce.probe.message_rate.mpps")
		.with_unit("Mpps")
		.with_description("Message rate from perftest summary row")
		.build();
	mrate.record(record.msg_rate_mpps.unwrap_or(0.0), &attrs);
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
	let otel_tracer = providers.tracer.tracer("accelrd-probe");
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
