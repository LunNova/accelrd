// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Daemon-subcommand args. argh's policy is "no env-var support in the
//! parser" — keeping CLI parsing and env handling separate. We respect
//! that by parsing CLI flags as `Option<T>` (i.e. "explicitly set?") and
//! folding env fallbacks + hardcoded defaults in a small `resolve()` step
//! before the daemon starts.

use argh::FromArgs;

/// Per-node daemon: sensor enumeration, preflight, OTLP, K8s node labels.
#[derive(FromArgs, Debug, Clone)]
#[argh(subcommand, name = "daemon")]
pub struct Args {
	/// OTLP/HTTP base endpoint, /v1/{traces,metrics,logs} appended
	/// (env: ACCELRD_OTLP_ENDPOINT; default: http://127.0.0.1:4318)
	#[argh(option)]
	pub otlp_endpoint: Option<String>,

	/// live telemetry emit interval, seconds (env: ACCELRD_LIVE_INTERVAL_SECS; default: 5)
	#[argh(option)]
	pub live_interval_secs: Option<u64>,

	/// preflight cycle interval, seconds (env: ACCELRD_PREFLIGHT_INTERVAL_SECS; default: 30)
	#[argh(option)]
	pub preflight_interval_secs: Option<u64>,

	/// datacenter row / PDU domain — opaque ID, used to label the node (env: ACCELRD_BLOCK)
	#[argh(option)]
	pub block: Option<String>,

	/// leaf-switch (TOR) rack ID — opaque, must match across same-TOR nodes (env: ACCELRD_RACK)
	#[argh(option)]
	pub rack: Option<String>,

	/// override OTel `service.name` resource attribute (env: OTEL_SERVICE_NAME; default: accelrd)
	#[argh(option)]
	pub service_name: Option<String>,

	/// override the auto-detected node name (env: NODE_NAME)
	#[argh(option)]
	pub node_name: Option<String>,

	/// run one preflight cycle and one live snapshot, then exit
	#[argh(switch)]
	pub once: bool,

	/// skip the K8s labeler entirely even if a service-account token is present
	#[argh(switch)]
	pub no_k8s: bool,

	/// disable LLDP-based rack discovery; useful when the daemon lacks CAP_NET_RAW
	#[argh(switch)]
	pub no_lldp: bool,

	/// LLDP capture timeout in seconds — switches advertise every 30s, so 60
	/// catches at least one frame; 0 = skip (env: ACCELRD_LLDP_TIMEOUT_SECS; default: 60)
	#[argh(option)]
	pub lldp_timeout_secs: Option<u64>,
}

/// Args with all env/default fallbacks applied. Internal layer above
/// `Args` so the rest of the daemon can take plain values.
#[derive(Debug, Clone)]
pub struct Resolved {
	pub otlp_endpoint: String,
	pub live_interval_secs: u64,
	pub preflight_interval_secs: u64,
	pub block: Option<String>,
	pub rack: Option<String>,
	pub service_name: String,
	pub node_name: Option<String>,
	pub once: bool,
	pub no_k8s: bool,
	pub no_lldp: bool,
	pub lldp_timeout_secs: u64,
}

impl Args {
	pub fn resolve(self) -> Resolved {
		Resolved {
			otlp_endpoint: self
				.otlp_endpoint
				.or_else(|| env_string("ACCELRD_OTLP_ENDPOINT"))
				.unwrap_or_else(|| "http://127.0.0.1:4318".into()),
			live_interval_secs: self
				.live_interval_secs
				.or_else(|| env_u64("ACCELRD_LIVE_INTERVAL_SECS"))
				.unwrap_or(5),
			preflight_interval_secs: self
				.preflight_interval_secs
				.or_else(|| env_u64("ACCELRD_PREFLIGHT_INTERVAL_SECS"))
				.unwrap_or(30),
			block: self.block.or_else(|| env_string("ACCELRD_BLOCK")),
			rack: self.rack.or_else(|| env_string("ACCELRD_RACK")),
			service_name: self
				.service_name
				.or_else(|| env_string("OTEL_SERVICE_NAME"))
				.unwrap_or_else(|| "accelrd".into()),
			node_name: self.node_name.or_else(|| env_string("NODE_NAME")),
			once: self.once,
			no_k8s: self.no_k8s,
			no_lldp: self.no_lldp,
			lldp_timeout_secs: self
				.lldp_timeout_secs
				.or_else(|| env_u64("ACCELRD_LLDP_TIMEOUT_SECS"))
				.unwrap_or(60),
		}
	}
}

pub fn env_string(key: &str) -> Option<String> {
	std::env::var(key).ok().filter(|s| !s.is_empty())
}

pub fn env_u64(key: &str) -> Option<u64> {
	std::env::var(key).ok().and_then(|s| s.parse().ok())
}
