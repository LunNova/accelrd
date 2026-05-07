// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Write `accel-test.lunnova.dev/last-{rack,loopback}-*` annotations on
//! probed nodes via RFC 7396 merge patches against
//! `metadata.annotations`, touching only our own keys. Two key families
//! share the patch-body shape:
//!   - `last-rack-*`     — cross-node pair probe (writes both nodes)
//!   - `last-loopback-*` — single-node ibverbs self-test (writes one)
//! The cluster-table reader on the admin side knows about both and
//! falls through pair → loopback → preflight when displaying a verdict.

use anyhow::Context;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, Patch, PatchParams};
use serde_json::{Value, json};

use crate::prober::results::ProbeRecord;
use crate::time::now_rfc3339;

pub const ANN_LAST_AT: &str = "accel-test.lunnova.dev/last-rack-at";
pub const ANN_LB_LAST_AT: &str = "accel-test.lunnova.dev/last-loopback-at";

struct Keys {
	last_at: &'static str,
	last_partner: Option<&'static str>,
	last_bw_avg: &'static str,
	last_bw_peak: &'static str,
	last_msg_rate: &'static str,
	last_device: &'static str,
	last_verdict: &'static str,
	last_reason: &'static str,
}

const RACK: Keys = Keys {
	last_at: "accel-test.lunnova.dev/last-rack-at",
	last_partner: Some("accel-test.lunnova.dev/last-rack-partner"),
	last_bw_avg: "accel-test.lunnova.dev/last-rack-bw-gbps",
	last_bw_peak: "accel-test.lunnova.dev/last-rack-bw-peak-gbps",
	last_msg_rate: "accel-test.lunnova.dev/last-rack-msg-rate-mpps",
	last_device: "accel-test.lunnova.dev/last-rack-device",
	last_verdict: "accel-test.lunnova.dev/last-rack-verdict",
	last_reason: "accel-test.lunnova.dev/last-rack-reason",
};

const LOOPBACK: Keys = Keys {
	last_at: "accel-test.lunnova.dev/last-loopback-at",
	last_partner: None,
	last_bw_avg: "accel-test.lunnova.dev/last-loopback-bw-gbps",
	last_bw_peak: "accel-test.lunnova.dev/last-loopback-bw-peak-gbps",
	last_msg_rate: "accel-test.lunnova.dev/last-loopback-msg-rate-mpps",
	last_device: "accel-test.lunnova.dev/last-loopback-device",
	last_verdict: "accel-test.lunnova.dev/last-loopback-verdict",
	last_reason: "accel-test.lunnova.dev/last-loopback-reason",
};

pub enum Verdict<'a> {
	Ok(&'a ProbeRecord),
	Fail(&'a str),
	Timeout,
}

pub async fn patch_pair(api: &Api<Node>, a: &str, b: &str, verdict: &Verdict<'_>) {
	let now = now_rfc3339();
	let body_a = build_body(&RACK, Some(b), &now, verdict);
	let body_b = build_body(&RACK, Some(a), &now, verdict);
	apply(api, a, &body_a).await;
	apply(api, b, &body_b).await;
}

pub async fn patch_loopback(api: &Api<Node>, node: &str, verdict: &Verdict<'_>) {
	let now = now_rfc3339();
	let body = build_body(&LOOPBACK, None, &now, verdict);
	apply(api, node, &body).await;
}

fn build_body(keys: &Keys, partner: Option<&str>, now: &str, verdict: &Verdict<'_>) -> Value {
	let mut anns: serde_json::Map<String, Value> = serde_json::Map::new();
	anns.insert(keys.last_at.into(), Value::String(now.into()));
	if let (Some(key), Some(p)) = (keys.last_partner, partner) {
		anns.insert(key.into(), Value::String(p.into()));
	}
	match verdict {
		Verdict::Ok(rec) => {
			anns.insert(keys.last_verdict.into(), Value::String("ok".into()));
			anns.insert(keys.last_reason.into(), Value::Null);
			anns.insert(keys.last_device.into(), Value::String(rec.device.clone()));
			if let Some(v) = rec.bw_avg_gbps {
				anns.insert(keys.last_bw_avg.into(), Value::String(format!("{v:.2}")));
			}
			if let Some(v) = rec.bw_peak_gbps {
				anns.insert(keys.last_bw_peak.into(), Value::String(format!("{v:.2}")));
			}
			if let Some(v) = rec.msg_rate_mpps {
				anns.insert(keys.last_msg_rate.into(), Value::String(format!("{v:.6}")));
			}
		}
		Verdict::Fail(reason) => {
			anns.insert(keys.last_verdict.into(), Value::String("fail".into()));
			anns.insert(keys.last_reason.into(), Value::String((*reason).into()));
			anns.insert(keys.last_bw_avg.into(), Value::Null);
			anns.insert(keys.last_bw_peak.into(), Value::Null);
			anns.insert(keys.last_msg_rate.into(), Value::Null);
		}
		Verdict::Timeout => {
			anns.insert(keys.last_verdict.into(), Value::String("timeout".into()));
			anns.insert(
				keys.last_reason.into(),
				Value::String("probe pod did not terminate before timeout".into()),
			);
			anns.insert(keys.last_bw_avg.into(), Value::Null);
			anns.insert(keys.last_bw_peak.into(), Value::Null);
			anns.insert(keys.last_msg_rate.into(), Value::Null);
		}
	}
	json!({ "metadata": { "annotations": anns } })
}

async fn apply(api: &Api<Node>, node: &str, body: &Value) {
	let res = api
		.patch(node, &PatchParams::default(), &Patch::Merge(body))
		.await
		.with_context(|| format!("patch node {node}"));
	if let Err(e) = res {
		tracing::warn!(error = %e, node = node, "annotation patch failed");
	} else {
		tracing::debug!(node = node, "annotations patched");
	}
}
