// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Probe subcommand: one side of a paired RoCE bandwidth test.
//!
//! Server mode runs `ib_send_bw -F` and waits for a client. Client mode
//! polls the server until it accepts a connection, then runs the same
//! command pointed at the server, parses the perftest output to extract
//! peak/average bandwidth, emits the result as an OTLP metric, and
//! prints a single JSON object on stdout for the prober to consume.

use std::str::FromStr;

use argh::FromArgs;

use crate::config::{env_string, env_u64};

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

	/// RDMA device name (e.g. mlx5_0); when unset, perftest's default device
	/// selection is used (env: ACCELRD_PROBE_DEVICE)
	#[argh(option)]
	pub device: Option<String>,

	/// test duration — wrapper-enforced upper bound, perftest controls iters
	/// (env: ACCELRD_PROBE_DURATION_SECS; default: 30)
	#[argh(option)]
	pub duration_secs: Option<u64>,

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
			duration_secs: self.duration_secs.or_else(|| env_u64("ACCELRD_PROBE_DURATION_SECS")).unwrap_or(30),
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

pub async fn run(_args: Resolved) -> anyhow::Result<()> {
	// TODO: implement.
	//
	// Server mode:
	//   - exec ib_send_bw -F (foreground; perftest prints "waiting for client")
	//   - wait for it to exit (client disconnect or signal)
	//   - emit the same JSON shape as the client (bandwidth seen on rx side)
	//   - exit
	//
	// Client mode:
	//   - retry-loop connect to server: TCP probe to perftest's port,
	//     give up after a deadline
	//   - exec ib_send_bw -F <server>
	//   - parse final summary line for bandwidth (Gb/s) and message rate
	//   - emit OTLP metric: roce.probe.bandwidth.gbps{src_rack, src_node, partner_node, mode=client}
	//   - print JSON {bandwidth_gbps, message_rate_mpps, duration_s, verdict} to stdout
	//   - exit 0 on success, non-zero on perftest failure
	tracing::warn!("probe subcommand not yet implemented");
	Ok(())
}
