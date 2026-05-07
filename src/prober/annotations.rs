// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Patch the two paired Nodes' `accel-test.lunnova.dev/last-rack-*`
//! annotations after a probe completes. Uses an RFC 7396 merge patch
//! against `metadata.annotations` so we only touch our own keys.

use anyhow::Context;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, Patch, PatchParams};
use serde_json::{Value, json};

use crate::prober::results::ProbeRecord;
use crate::time::now_rfc3339;

pub const ANN_LAST_AT: &str = "accel-test.lunnova.dev/last-rack-at";
pub const ANN_LAST_PARTNER: &str = "accel-test.lunnova.dev/last-rack-partner";
pub const ANN_LAST_BW_AVG: &str = "accel-test.lunnova.dev/last-rack-bw-gbps";
pub const ANN_LAST_BW_PEAK: &str = "accel-test.lunnova.dev/last-rack-bw-peak-gbps";
pub const ANN_LAST_MSG_RATE: &str = "accel-test.lunnova.dev/last-rack-msg-rate-mpps";
pub const ANN_LAST_DEVICE: &str = "accel-test.lunnova.dev/last-rack-device";
pub const ANN_LAST_VERDICT: &str = "accel-test.lunnova.dev/last-rack-verdict";
pub const ANN_LAST_REASON: &str = "accel-test.lunnova.dev/last-rack-reason";

pub enum Verdict<'a> {
	Ok(&'a ProbeRecord),
	Fail(&'a str),
	Timeout,
}

pub async fn patch_pair(api: &Api<Node>, a: &str, b: &str, verdict: &Verdict<'_>) {
	let now = now_rfc3339();
	let body_a = build_body(b, &now, verdict);
	let body_b = build_body(a, &now, verdict);
	apply(api, a, &body_a).await;
	apply(api, b, &body_b).await;
}

fn build_body(partner: &str, now: &str, verdict: &Verdict<'_>) -> Value {
	let mut anns: serde_json::Map<String, Value> = serde_json::Map::new();
	anns.insert(ANN_LAST_AT.into(), Value::String(now.into()));
	anns.insert(ANN_LAST_PARTNER.into(), Value::String(partner.into()));
	match verdict {
		Verdict::Ok(rec) => {
			anns.insert(ANN_LAST_VERDICT.into(), Value::String("ok".into()));
			anns.insert(ANN_LAST_REASON.into(), Value::Null);
			anns.insert(ANN_LAST_DEVICE.into(), Value::String(rec.device.clone()));
			if let Some(v) = rec.bw_avg_gbps {
				anns.insert(ANN_LAST_BW_AVG.into(), Value::String(format!("{v:.2}")));
			}
			if let Some(v) = rec.bw_peak_gbps {
				anns.insert(ANN_LAST_BW_PEAK.into(), Value::String(format!("{v:.2}")));
			}
			if let Some(v) = rec.msg_rate_mpps {
				anns.insert(ANN_LAST_MSG_RATE.into(), Value::String(format!("{v:.6}")));
			}
		}
		Verdict::Fail(reason) => {
			anns.insert(ANN_LAST_VERDICT.into(), Value::String("fail".into()));
			anns.insert(ANN_LAST_REASON.into(), Value::String((*reason).into()));
			anns.insert(ANN_LAST_BW_AVG.into(), Value::Null);
			anns.insert(ANN_LAST_BW_PEAK.into(), Value::Null);
			anns.insert(ANN_LAST_MSG_RATE.into(), Value::Null);
		}
		Verdict::Timeout => {
			anns.insert(ANN_LAST_VERDICT.into(), Value::String("timeout".into()));
			anns.insert(
				ANN_LAST_REASON.into(),
				Value::String("probe pod did not terminate before timeout".into()),
			);
			anns.insert(ANN_LAST_BW_AVG.into(), Value::Null);
			anns.insert(ANN_LAST_BW_PEAK.into(), Value::Null);
			anns.insert(ANN_LAST_MSG_RATE.into(), Value::Null);
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
