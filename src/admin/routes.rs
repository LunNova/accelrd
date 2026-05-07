// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Axum router for the admin console. JSON `/api/*` endpoints + a
//! single static SPA mounted at `/`.

use std::collections::BTreeMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower_http::compression::CompressionLayer;

use super::assets;
use super::k8s::{self as admin_k8s, ClusterSummary, NodeView, VerdictSource};
use super::mutel::{MetricQueryParams, NodeRollup};
use super::state::AppState;

/// Window for "latest" mutel queries. The daemon emits live metrics
/// every 5s by default, so 5 minutes comfortably covers a temporary
/// ingest gap without hiding real outages.
const FLEET_WINDOW_SECS: u64 = 300;

pub fn router(state: AppState) -> Router {
	Router::new()
		.route("/", get(serve_index))
		.route("/static/{*rest}", get(serve_static))
		.route("/api/health", get(health))
		.route("/api/cluster", get(cluster))
		.route("/api/probes", get(probes))
		.route("/api/fleet", get(fleet))
		.route("/api/metrics/list", get(metrics_list))
		.route("/api/metrics/query/{name}", get(metric_query))
		.route("/api/logs/recent", get(logs_recent))
		.fallback(not_found)
		.layer(CompressionLayer::new())
		.with_state(state)
}

async fn serve_index() -> Response {
	assets::index()
}

async fn serve_static(Path(rest): Path<String>) -> Response {
	assets::serve(&rest)
}

async fn not_found() -> Response {
	(StatusCode::NOT_FOUND, "Not found").into_response()
}

#[derive(Serialize)]
struct ServiceStatus {
	ok: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	error: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
	uptime_secs: u64,
	k8s: ServiceStatus,
	mutel: ServiceStatus,
	mutel_endpoint: String,
	version: &'static str,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
	let uptime_secs = state.started_at.elapsed().as_secs();

	let k8s = match &state.kube {
		None => ServiceStatus {
			ok: false,
			error: Some("k8s client disabled or unavailable".into()),
		},
		Some(client) => {
			use k8s_openapi::api::core::v1::Node;
			use kube::Api;
			let api: Api<Node> = Api::all(client.clone());
			match api.list(&kube::api::ListParams::default().limit(1)).await {
				Ok(_) => ServiceStatus { ok: true, error: None },
				Err(e) => ServiceStatus {
					ok: false,
					error: Some(e.to_string()),
				},
			}
		}
	};

	let mutel = match state.mutel.ping().await {
		Ok(()) => ServiceStatus { ok: true, error: None },
		Err(e) => ServiceStatus {
			ok: false,
			error: Some(e.to_string()),
		},
	};

	Json(HealthResponse {
		uptime_secs,
		k8s,
		mutel,
		mutel_endpoint: state.mutel.base().to_string(),
		version: env!("CARGO_PKG_VERSION"),
	})
}

#[derive(Serialize)]
struct ClusterResponse {
	nodes: Vec<NodeView>,
	summary: ClusterSummary,
}

async fn cluster(State(state): State<AppState>) -> Result<Json<ClusterResponse>, ApiError> {
	let nodes = load_nodes(&state).await?;
	let summary = admin_k8s::summarize(&nodes);
	Ok(Json(ClusterResponse { nodes, summary }))
}

#[derive(Serialize)]
struct ProbesResponse {
	probes: Vec<ProbeRow>,
}

#[derive(Serialize)]
struct ProbeRow {
	node: String,
	rack: Option<String>,
	at: Option<String>,
	bandwidth_gbps: Option<f64>,
	partner: Option<String>,
	verdict: Option<String>,
	source: VerdictSource,
}

async fn probes(State(state): State<AppState>) -> Result<Json<ProbesResponse>, ApiError> {
	let nodes = load_nodes(&state).await?;
	let mut probes: Vec<ProbeRow> = nodes
		.into_iter()
		.filter_map(|n| {
			let p = n.last_probe?;
			Some(ProbeRow {
				node: n.name,
				rack: n.rack,
				at: p.at,
				bandwidth_gbps: p.bandwidth_gbps,
				partner: p.partner,
				verdict: p.verdict,
				source: p.source,
			})
		})
		.collect();
	// Most recent first. Lexicographic sort works for RFC-3339 timestamps,
	// which is what the prober writes.
	probes.sort_by(|a, b| b.at.cmp(&a.at));
	Ok(Json(ProbesResponse { probes }))
}

/// Live fleet gauges, parallel-fetched from mutel and reduced into a
/// single response the SPA can consume in one round-trip.
///
/// Per-node values come straight from the latest mutel sample (within
/// `FLEET_WINDOW_SECS`); fleet aggregates are computed locally. NaN /
/// infinite values are filtered out by `latest_by_node`.
#[derive(Serialize)]
struct FleetResponse {
	available: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	error: Option<String>,
	window_secs: u64,
	by_node: BTreeMap<String, NodeFleetMetrics>,
	fleet: FleetTotals,
}

#[derive(Serialize, Default)]
struct NodeFleetMetrics {
	#[serde(skip_serializing_if = "Option::is_none")]
	mem_used_bytes: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	mem_total_bytes: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	ram_total_bytes: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	ram_available_bytes: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	disk_free_bytes: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	disk_total_bytes: Option<f64>,
	/// GPU + host power summed. The GPU contribution is `accel.power.usage`
	/// (amdgpu hwmon `power1_average`); the host contribution is
	/// `host.power.usage` (any non-GPU hwmon `power*_input` — typically
	/// `amd_energy`, IPMI BMC, or PSU). On nodes with neither hwmon loaded
	/// the host portion is zero and the field reflects GPU draw alone.
	#[serde(skip_serializing_if = "Option::is_none")]
	power_watts: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	temp_c: Option<f64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	utilization: Option<f64>,
}

#[derive(Serialize, Default)]
struct FleetTotals {
	mem_used_bytes: f64,
	mem_total_bytes: f64,
	ram_total_bytes: f64,
	ram_available_bytes: f64,
	disk_free_bytes: f64,
	power_watts: f64,
	max_temp_c: Option<f64>,
	avg_utilization: Option<f64>,
	nodes_with_data: usize,
}

async fn fleet(State(state): State<AppState>) -> Json<FleetResponse> {
	// Single accelerator-memory family: we previously split into
	// `accel.memory.{dedicated,unified}.*` based on a sysfs heuristic
	// (GTT-dwarfs-VRAM ⇒ Unified) but the heuristic misclassified
	// servers with large host RAM where GTT routinely exceeds VRAM
	// even on discrete cards. Treating both as one bucket is the
	// honest aggregate — `accel.memory.{total,used}` from amdgpu's
	// `mem_info_vram_*`, and BAR-derived totals on nvidia/intel.
	let mem_used = state
		.mutel
		.latest_by_node("accel.memory.used", NodeRollup::Sum, FLEET_WINDOW_SECS);
	let mem_total = state
		.mutel
		.latest_by_node("accel.memory.total", NodeRollup::Sum, FLEET_WINDOW_SECS);
	// Host metrics: one series per node (no per-card multiplicity), so
	// Max and Sum collapse to the same number — Max is just the
	// "principle of least surprise" choice.
	let ram_total = state
		.mutel
		.latest_by_node("host.memory.total_bytes", NodeRollup::Max, FLEET_WINDOW_SECS);
	let ram_avail = state
		.mutel
		.latest_by_node("host.memory.available_bytes", NodeRollup::Max, FLEET_WINDOW_SECS);
	// Disk: per-mount series; Max picks the biggest mount on the node,
	// which is the workload-relevant figure.
	let disk_free = state
		.mutel
		.latest_by_node("host.disk.free_bytes", NodeRollup::Max, FLEET_WINDOW_SECS);
	let disk_total = state
		.mutel
		.latest_by_node("host.disk.total_bytes", NodeRollup::Max, FLEET_WINDOW_SECS);
	// Power: GPU draw + host draw, summed per node. The GPU series is
	// `accel.power.usage` (one sample per card, summed across cards on
	// a node); the host series is `host.power.usage` (one sample per
	// non-GPU hwmon `power*_input` rail, summed across rails). Either
	// stream may be empty on a given node — only nodes with neither
	// stream have a None power_watts.
	let gpu_power = state
		.mutel
		.latest_by_node("accel.power.usage", NodeRollup::Sum, FLEET_WINDOW_SECS);
	let host_power = state
		.mutel
		.latest_by_node("host.power.usage", NodeRollup::Sum, FLEET_WINDOW_SECS);
	let temp = state
		.mutel
		.latest_by_node("accel.temperature", NodeRollup::Max, FLEET_WINDOW_SECS);
	let util = state
		.mutel
		.latest_by_node("accel.utilization", NodeRollup::Avg, FLEET_WINDOW_SECS);

	let (mu, mt, rt, ra, df, dtot, gpw, hpw, tmp, util_) = tokio::join!(
		mem_used, mem_total, ram_total, ram_avail, disk_free, disk_total, gpu_power, host_power, temp, util
	);

	// First failure wins: if mutel is unreachable we mark the whole
	// payload as unavailable but still return zeroed structure so the
	// SPA can render placeholders without special-casing nulls.
	let mut error: Option<String> = None;
	let mut pick = |r: anyhow::Result<BTreeMap<String, f64>>| -> BTreeMap<String, f64> {
		match r {
			Ok(m) => m,
			Err(e) => {
				if error.is_none() {
					error = Some(format!("{e:#}"));
				}
				BTreeMap::new()
			}
		}
	};
	let mu = pick(mu);
	let mt = pick(mt);
	let rt = pick(rt);
	let ra = pick(ra);
	let df = pick(df);
	let dtot = pick(dtot);
	let gpw = pick(gpw);
	let hpw = pick(hpw);
	let tmp = pick(tmp);
	let util_ = pick(util_);

	let mut nodes: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
	for k in mu.keys().chain(mt.keys()) {
		nodes.insert(k.clone());
	}
	for k in rt.keys().chain(ra.keys()).chain(df.keys()).chain(dtot.keys()) {
		nodes.insert(k.clone());
	}
	for k in gpw.keys().chain(hpw.keys()).chain(tmp.keys()).chain(util_.keys()) {
		nodes.insert(k.clone());
	}

	let mut by_node: BTreeMap<String, NodeFleetMetrics> = BTreeMap::new();
	let mut totals = FleetTotals::default();
	let mut util_acc = 0.0f64;
	let mut util_n = 0usize;

	for node in nodes {
		let power_watts = match (gpw.get(&node), hpw.get(&node)) {
			(None, None) => None,
			(g, h) => Some(g.copied().unwrap_or(0.0) + h.copied().unwrap_or(0.0)),
		};
		let m = NodeFleetMetrics {
			mem_used_bytes: mu.get(&node).copied(),
			mem_total_bytes: mt.get(&node).copied(),
			ram_total_bytes: rt.get(&node).copied(),
			ram_available_bytes: ra.get(&node).copied(),
			disk_free_bytes: df.get(&node).copied(),
			disk_total_bytes: dtot.get(&node).copied(),
			power_watts,
			temp_c: tmp.get(&node).copied(),
			utilization: util_.get(&node).copied(),
		};
		if let Some(v) = m.mem_used_bytes {
			totals.mem_used_bytes += v;
		}
		if let Some(v) = m.mem_total_bytes {
			totals.mem_total_bytes += v;
		}
		if let Some(v) = m.ram_total_bytes {
			totals.ram_total_bytes += v;
		}
		if let Some(v) = m.ram_available_bytes {
			totals.ram_available_bytes += v;
		}
		if let Some(v) = m.disk_free_bytes {
			totals.disk_free_bytes += v;
		}
		if let Some(v) = m.power_watts {
			totals.power_watts += v;
		}
		if let Some(v) = m.temp_c {
			totals.max_temp_c = Some(totals.max_temp_c.map_or(v, |cur| cur.max(v)));
		}
		if let Some(v) = m.utilization {
			util_acc += v;
			util_n += 1;
		}
		totals.nodes_with_data += 1;
		by_node.insert(node, m);
	}
	if util_n > 0 {
		totals.avg_utilization = Some(util_acc / util_n as f64);
	}

	let available = error.is_none();
	Json(FleetResponse {
		available,
		error,
		window_secs: FLEET_WINDOW_SECS,
		by_node,
		fleet: totals,
	})
}

async fn metrics_list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
	let m = state.mutel.list_metrics().await.map_err(ApiError::upstream)?;
	Ok(Json(serde_json::to_value(m).unwrap_or(json!({"metrics": []}))))
}

#[derive(Deserialize)]
struct MetricQueryQs {
	filter: Option<String>,
	group_by: Option<String>,
	agg: Option<String>,
	start: Option<i64>,
	end: Option<i64>,
	max_points: Option<u32>,
}

async fn metric_query(
	State(state): State<AppState>,
	Path(name): Path<String>,
	Query(qs): Query<MetricQueryQs>,
) -> Result<Json<Value>, ApiError> {
	let params = MetricQueryParams {
		filter: qs.filter,
		group_by: qs.group_by,
		agg: qs.agg,
		start: qs.start,
		end: qs.end,
		max_points: qs.max_points,
	};
	let v = state
		.mutel
		.query_metric(&name, &params)
		.await
		.map_err(ApiError::upstream)?;
	Ok(Json(v))
}

#[derive(Deserialize)]
struct LogsQs {
	service: Option<String>,
	limit: Option<u32>,
}

async fn logs_recent(State(state): State<AppState>, Query(qs): Query<LogsQs>) -> Result<Json<Value>, ApiError> {
	let limit = qs.limit.unwrap_or(100).min(1000);
	let v = state
		.mutel
		.recent_logs(qs.service.as_deref(), limit)
		.await
		.map_err(ApiError::upstream)?;
	Ok(Json(v))
}

async fn load_nodes(state: &AppState) -> Result<Vec<NodeView>, ApiError> {
	match &state.kube {
		None => Ok(Vec::new()),
		Some(client) => admin_k8s::list_nodes(client).await.map_err(ApiError::cluster),
	}
}

/// Uniform JSON error envelope. Keeps the SPA's error handling
/// simple (`{ error: { source, message } }`).
struct ApiError {
	status: StatusCode,
	source: &'static str,
	message: String,
}

impl ApiError {
	fn cluster(e: anyhow::Error) -> Self {
		Self {
			status: StatusCode::BAD_GATEWAY,
			source: "cluster",
			message: format!("{e:#}"),
		}
	}
	fn upstream(e: anyhow::Error) -> Self {
		Self {
			status: StatusCode::BAD_GATEWAY,
			source: "mutel",
			message: format!("{e:#}"),
		}
	}
}

impl IntoResponse for ApiError {
	fn into_response(self) -> Response {
		(
			self.status,
			Json(json!({ "error": { "source": self.source, "message": self.message } })),
		)
			.into_response()
	}
}
