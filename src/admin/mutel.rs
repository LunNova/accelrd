// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Thin client for the mutel HTTP API. We only need a small slice of
//! it — list metrics, query a single metric with filters/grouping, and
//! poll recent logs. Errors are surfaced as `anyhow::Error` so the
//! handler layer can render a UI banner instead of a 500 page.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct MutelClient {
	http: Client,
	base: String,
}

impl MutelClient {
	pub fn new(base: String, timeout_secs: u64) -> anyhow::Result<Self> {
		let http = Client::builder()
			.timeout(Duration::from_secs(timeout_secs))
			.user_agent(concat!("accelrd-admin/", env!("CARGO_PKG_VERSION")))
			.build()
			.context("build reqwest client")?;
		Ok(Self {
			http,
			base: base.trim_end_matches('/').to_string(),
		})
	}

	pub fn base(&self) -> &str {
		&self.base
	}

	pub async fn list_metrics(&self) -> anyhow::Result<MetricsList> {
		let url = format!("{}/api/metrics", self.base);
		let resp = self.http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
		let resp = resp.error_for_status().with_context(|| format!("GET {url} status"))?;
		resp.json::<MetricsList>()
			.await
			.with_context(|| format!("GET {url} json"))
	}

	pub async fn query_metric(&self, name: &str, params: &MetricQueryParams) -> anyhow::Result<serde_json::Value> {
		let url = format!("{}/api/metrics/{}", self.base, urlencode(name));
		let req = self.http.get(&url).query(&params.to_pairs());
		let resp = req.send().await.with_context(|| format!("GET {url}"))?;
		let resp = resp.error_for_status().with_context(|| format!("GET {url} status"))?;
		resp.json::<serde_json::Value>()
			.await
			.with_context(|| format!("GET {url} json"))
	}

	pub async fn recent_logs(&self, service: Option<&str>, limit: u32) -> anyhow::Result<serde_json::Value> {
		let url = format!("{}/api/logs/recent", self.base);
		let mut req = self.http.get(&url).query(&[("limit", limit.to_string())]);
		if let Some(s) = service {
			// /api/logs supports `search` (substring); we apply a simple
			// substring-on-service filter via the search param.
			req = req.query(&[("search", s.to_string())]);
		}
		let resp = req.send().await.with_context(|| format!("GET {url}"))?;
		let resp = resp.error_for_status().with_context(|| format!("GET {url} status"))?;
		resp.json::<serde_json::Value>()
			.await
			.with_context(|| format!("GET {url} json"))
	}

	/// Query a metric, group by `r:k8s.node.name`, and return the latest
	/// value per node over the last `window_secs` seconds. Returns a
	/// `(node_name → value)` map. Nodes with no data in the window are
	/// absent from the map. Uses `max_points=1` so mutel collapses the
	/// whole window into one bucketed value per group.
	pub async fn latest_by_node(
		&self,
		name: &str,
		agg: &str,
		window_secs: u64,
	) -> anyhow::Result<BTreeMap<String, f64>> {
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.context("system time")?
			.as_secs() as i64;
		let params = MetricQueryParams {
			group_by: Some("r:k8s.node.name".into()),
			agg: Some(agg.into()),
			start: Some(now.saturating_sub(window_secs as i64)),
			end: Some(now),
			max_points: Some(1),
			..Default::default()
		};
		let v = self.query_metric(name, &params).await?;
		let mut out = BTreeMap::new();
		let groups = v.get("groups").and_then(|g| g.as_array()).cloned().unwrap_or_default();
		for g in groups {
			let node = g
				.get("key")
				.and_then(|k| k.get("r:k8s.node.name"))
				.and_then(|v| v.as_str())
				.unwrap_or("")
				.to_string();
			if node.is_empty() {
				continue;
			}
			let value = g
				.get("points")
				.and_then(|p| p.as_array())
				.and_then(|points| points.last())
				.and_then(|pt| pt.as_array())
				.and_then(|tuple| tuple.get(1))
				.and_then(|v| v.as_f64());
			if let Some(v) = value
				&& v.is_finite()
			{
				out.insert(node, v);
			}
		}
		Ok(out)
	}

	pub async fn ping(&self) -> anyhow::Result<()> {
		// `/api/metrics` is the cheapest "is mutel up" probe we have.
		// HEAD isn't supported on the route, so do a small GET.
		let url = format!("{}/api/metrics", self.base);
		let resp = self.http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
		resp.error_for_status().with_context(|| format!("GET {url} status"))?;
		Ok(())
	}
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MetricsList {
	pub metrics: Vec<MetricSummary>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MetricSummary {
	pub name: String,
	#[serde(default)]
	pub kind: Option<String>,
	#[serde(default)]
	pub unit: Option<String>,
	#[serde(default)]
	pub series_count: Option<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct MetricQueryParams {
	pub filter: Option<String>,
	pub group_by: Option<String>,
	pub agg: Option<String>,
	pub start: Option<i64>,
	pub end: Option<i64>,
	pub max_points: Option<u32>,
}

impl MetricQueryParams {
	fn to_pairs(&self) -> Vec<(&'static str, String)> {
		let mut pairs = Vec::new();
		if let Some(v) = &self.filter {
			pairs.push(("filter", v.clone()));
		}
		if let Some(v) = &self.group_by {
			pairs.push(("group_by", v.clone()));
		}
		if let Some(v) = &self.agg {
			pairs.push(("agg", v.clone()));
		}
		if let Some(v) = self.start {
			pairs.push(("start", v.to_string()));
		}
		if let Some(v) = self.end {
			pairs.push(("end", v.to_string()));
		}
		if let Some(v) = self.max_points {
			pairs.push(("max_points", v.to_string()));
		}
		pairs
	}
}

/// Minimal RFC-3986 query-component encoding for path segments. Avoids
/// pulling in a full url crate — metric names in our world are
/// well-behaved (alphanumerics + dots + dashes).
fn urlencode(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for b in s.bytes() {
		match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
			_ => out.push_str(&format!("%{b:02X}")),
		}
	}
	out
}
