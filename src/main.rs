// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! accelrd — vendor-neutral accelerator readiness toolkit.
//!
//! One binary, three deployment shapes:
//!  * `accelrd daemon` — per-node DaemonSet. Reads accelerator state via
//!    sysfs / /proc only, emits live OTLP metrics/logs/traces, reconciles
//!    node labels for topology-aware schedulers.
//!  * `accelrd prober` — cluster-scoped controller that schedules paired
//!    RoCE bandwidth probes between nodes that share a topology rack.
//!  * `accelrd probe` — one side of a paired probe (server or client),
//!    wraps `ib_send_bw` / similar perftest tools, emits OTLP results.

mod config;
mod exporter;
mod k8s;
mod lifecycle;
mod preflight;
mod probe;
mod prober;
mod sensors;
mod telemetry;
mod topology;

use argh::FromArgs;

/// Vendor-neutral accelerator readiness toolkit.
#[derive(FromArgs)]
struct Cli {
	#[argh(subcommand)]
	cmd: Cmd,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Cmd {
	Daemon(config::Args),
	Prober(prober::ProberArgs),
	Probe(probe::ProbeArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let cli: Cli = argh::from_env();
	match cli.cmd {
		Cmd::Daemon(args) => lifecycle::run(args.resolve()).await,
		Cmd::Prober(args) => prober::run(args.resolve()).await,
		Cmd::Probe(args) => probe::run(args.resolve()).await,
	}
}
