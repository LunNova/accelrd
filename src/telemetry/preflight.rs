// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Preflight cycle loop. Each tick:
//!  - open a parent span "preflight_cycle"
//!  - run check × accelerator matrix where applies_to() is true
//!  - emit per-check duration + status gauges
//!  - aggregate per-readiness-class verdict
//!  - reconcile k8s labels
//!  - log a structured summary

use std::time::{Duration, Instant};

use opentelemetry::{KeyValue, global, trace::Tracer};

use crate::k8s::labeler::OptionalLabeler;
use crate::preflight::{CheckContext, PreflightCheck, Status, aggregate};
use crate::sensors::Accelerator;
use crate::telemetry::live::base_attrs;
use crate::topology::NodeTopology;
use crate::topology::labels::LabelSet;

pub struct PreflightInputs {
	pub checks: Vec<Box<dyn PreflightCheck>>,
	pub accelerators: Vec<Accelerator>,
	pub topology: NodeTopology,
	pub interval: Duration,
	pub once: bool,
	pub label_base: LabelSet,
}

pub async fn run(inputs: PreflightInputs, labeler: OptionalLabeler) {
	let meter = global::meter("accel-readiness.preflight");
	let duration_g = meter
		.f64_gauge("accel.preflight.check.duration_ms")
		.with_unit("ms")
		.with_description("Wall-clock duration of one preflight check on one accelerator.")
		.build();
	let pass_g = meter
		.f64_gauge("accel.preflight.check.pass")
		.with_unit("1")
		.with_description("1 if the check Passed; 0 for Warn/Fail; not emitted for Skipped.")
		.build();

	let tracer = global::tracer("accel-readiness.preflight");

	loop {
		let mut span = tracer.start("preflight_cycle");
		let mut all_outcomes = Vec::new();
		let mut failed_checks = Vec::new();

		for check in &inputs.checks {
			for accel in &inputs.accelerators {
				if !check.applies_to(accel) {
					continue;
				}
				let mut child = tracer.start(format!("check.{}", check.name()));
				child.set_attribute(KeyValue::new("check.name", check.name()));
				child.set_attribute(KeyValue::new("check.scope", check.scope().slug()));
				child.set_attribute(KeyValue::new("accel.vendor", accel.id.vendor.slug()));
				child.set_attribute(KeyValue::new("accel.model", accel.model.clone()));
				child.set_attribute(KeyValue::new("accel.index", accel.id.drm_index as i64));

				let ctx = CheckContext { accelerator: Some(accel) };
				let started = Instant::now();
				let outcome = check.run(&ctx).await;
				let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

				let mut attrs = base_attrs(accel);
				attrs.push(KeyValue::new("name", check.name()));
				attrs.push(KeyValue::new("status", outcome.status.slug()));

				duration_g.record(elapsed_ms, &attrs);
				if outcome.status != Status::Skipped {
					pass_g.record(if outcome.status == Status::Pass { 1.0 } else { 0.0 }, &attrs);
				}

				if matches!(outcome.status, Status::Fail | Status::Warn) {
					failed_checks.push(format!("{}@{}", check.name(), accel.id.drm_index));
				}
				all_outcomes.push(outcome.status);

				child.set_attribute(KeyValue::new("status", outcome.status.slug()));
				if let Some(msg) = &outcome.message {
					child.set_attribute(KeyValue::new("message", msg.clone()));
				}
				use opentelemetry::trace::Span;
				child.end();
			}
		}

		let inference = aggregate(&all_outcomes);
		let training = inference; // same set today; differentiate when real checks land

		let mut labels = inputs.label_base.clone();
		labels.labels.insert("accel-ready.lunnova.dev/inference".into(), inference.label_value().into());
		labels.labels.insert("accel-ready.lunnova.dev/training".into(), training.label_value().into());
		labels.annotations.insert("accel-ready.lunnova.dev/last-check".into(), now_rfc3339());
		if !failed_checks.is_empty() {
			labels.annotations.insert("accel-ready.lunnova.dev/failed".into(), failed_checks.join(","));
		}

		labeler.reconcile(&labels).await;

		tracing::info!(
			inference = %inference.label_value(),
			training = %training.label_value(),
			ran = all_outcomes.len(),
			failed = failed_checks.len(),
			fabric_domains = inputs.topology.fabric_domains.len(),
			"preflight_cycle complete",
		);

		use opentelemetry::trace::Span;
		span.set_attribute(KeyValue::new("inference", inference.label_value()));
		span.set_attribute(KeyValue::new("training", training.label_value()));
		span.end();

		if inputs.once {
			return;
		}
		tokio::time::sleep(inputs.interval).await;
	}
}

fn now_rfc3339() -> String {
	use std::time::{SystemTime, UNIX_EPOCH};
	let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
	let secs = dur.as_secs();
	let (s, m, h) = ((secs % 60), (secs / 60) % 60, (secs / 3600) % 24);
	let days = (secs / 86400) as i64;
	let (y, mo, d) = days_to_ymd(days);
	format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's date algorithm — `days_since_epoch` is 1970-01-01 == 0.
/// See <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>.
fn days_to_ymd(days_since_epoch: i64) -> (i64, u32, u32) {
	let z = days_since_epoch + 719_468;
	let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
	let doe = (z - era * 146_097) as u64;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let y = yoe as i64 + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let m = if mp < 10 { mp + 3 } else { mp - 9 };
	let y = if m <= 2 { y + 1 } else { y };
	(y, m as u32, d as u32)
}
