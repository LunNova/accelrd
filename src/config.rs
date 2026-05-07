// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
	name = "accelrd",
	about = "Vendor-neutral accelerator readiness daemon (sysfs-only sensors, OTLP telemetry, K8s labels)"
)]
pub struct Args {
	/// OTLP/HTTP base endpoint. Per-signal paths /v1/{traces,metrics,logs} are appended.
	#[arg(long, default_value = "http://127.0.0.1:4318", env = "ACCELRD_OTLP_ENDPOINT")]
	pub otlp_endpoint: String,

	/// Live telemetry emit interval, seconds.
	#[arg(long, default_value_t = 5, env = "ACCELRD_LIVE_INTERVAL_SECS")]
	pub live_interval_secs: u64,

	/// Preflight cycle interval, seconds.
	#[arg(long, default_value_t = 30, env = "ACCELRD_PREFLIGHT_INTERVAL_SECS")]
	pub preflight_interval_secs: u64,

	/// Datacenter row / PDU domain. Opaque ID, used to label the node.
	#[arg(long, env = "ACCELRD_BLOCK")]
	pub block: Option<String>,

	/// Leaf-switch (TOR) rack ID. Opaque, must match across nodes on the same TOR.
	#[arg(long, env = "ACCELRD_RACK")]
	pub rack: Option<String>,

	/// Override OTel `service.name` resource attribute.
	#[arg(long, default_value = "accelrd", env = "OTEL_SERVICE_NAME")]
	pub service_name: String,

	/// Override the auto-detected node name (defaults to `gethostname` / NODE_NAME env).
	#[arg(long, env = "NODE_NAME")]
	pub node_name: Option<String>,

	/// Run one preflight cycle and one live snapshot, then exit. Used in tests.
	#[arg(long)]
	pub once: bool,

	/// Skip the K8s labeler entirely even if a service-account token is present.
	#[arg(long)]
	pub no_k8s: bool,

	/// Disable LLDP-based rack discovery. Useful when the daemon lacks
	/// CAP_NET_RAW and you'd rather skip the wait than log noise.
	#[arg(long)]
	pub no_lldp: bool,

	/// LLDP capture timeout in seconds. Default switches advertise every 30s,
	/// so 60 catches at least one frame; bump higher in lossy networks. Set
	/// to 0 to skip discovery (equivalent to --no-lldp).
	#[arg(long, default_value_t = 60, env = "ACCELRD_LLDP_TIMEOUT_SECS")]
	pub lldp_timeout_secs: u64,
}
