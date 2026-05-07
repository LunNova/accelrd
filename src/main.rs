// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! accel-readiness — vendor-neutral accelerator readiness daemon.
//!
//! Reads accelerator state via sysfs / /proc only (no NVML, no ROCm,
//! no nvidia-smi shelling). Emits live OTLP metrics/logs/traces and,
//! when running inside a Kubernetes pod, reconciles node labels for
//! topology-aware schedulers.

mod config;
mod exporter;
mod k8s;
mod lifecycle;
mod preflight;
mod sensors;
mod telemetry;
mod topology;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let args = config::Args::parse();
	lifecycle::run(args).await
}
