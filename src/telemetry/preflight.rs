// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Preflight cycle loop. Each tick:
//!  - open a parent `preflight_cycle` tracing span
//!  - run check × accelerator matrix where applies_to() is true; each
//!    invocation gets its own `preflight.check` child span via
//!    `Instrument::instrument`, so the OTel exporter sees a proper
//!    parent → child trace tree
//!  - emit per-check duration + status gauges
//!  - aggregate per-readiness-class verdict
//!  - reconcile k8s labels
//!  - log a structured summary

use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Gauge, Meter};
use tracing::{Instrument, field};

use crate::k8s::labeler::OptionalLabeler;
use crate::preflight::{CheckContext, CheckOutcome, NodeReadiness, PreflightCheck, Status, aggregate};
use crate::sensors::Accelerator;
use crate::telemetry::{GaugeCache, base_attrs};
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

/// Cached gauge instruments + observation cache for a preflight loop.
/// Bundles the per-cycle metric state so `run_cycle` doesn't take six
/// separate refs.
struct PreflightMetrics {
	meter: Meter,
	duration_g: Gauge<f64>,
	pass_g: Gauge<f64>,
	obs_cache: GaugeCache,
}

impl PreflightMetrics {
	fn new() -> Self {
		let meter = global::meter("accelrd.preflight");
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
		Self { meter, duration_g, pass_g, obs_cache: GaugeCache::default() }
	}

	fn record(&mut self, check: &dyn PreflightCheck, accel: &Accelerator, outcome: &CheckOutcome, elapsed_ms: f64) {
		let mut attrs = base_attrs(accel);
		attrs.push(KeyValue::new("name", check.name()));
		attrs.push(KeyValue::new("status", outcome.status.slug()));

		self.duration_g.record(elapsed_ms, &attrs);
		if outcome.status != Status::Skipped {
			self.pass_g.record(if outcome.status == Status::Pass { 1.0 } else { 0.0 }, &attrs);
		}

		for m in &outcome.measurements {
			let mut obs_attrs = base_attrs(accel);
			obs_attrs.push(KeyValue::new("check", check.name()));
			obs_attrs.extend(m.attrs.iter().map(|(k, v)| KeyValue::new(k.to_string(), v.clone())));
			self.obs_cache.get_or_create(&self.meter, m).record(m.value, &obs_attrs);
		}
	}
}

pub async fn run(inputs: PreflightInputs, labeler: OptionalLabeler) {
	let mut metrics = PreflightMetrics::new();

	loop {
		let cycle_span = tracing::info_span!(
			"preflight_cycle",
			inference = field::Empty,
			training = field::Empty,
			ran = field::Empty,
			failed = field::Empty,
		);

		let cycle = run_cycle(&inputs, &mut metrics, &labeler).instrument(cycle_span.clone()).await;

		cycle_span.record("inference", cycle.inference.label_value());
		cycle_span.record("training", cycle.training.label_value());
		cycle_span.record("ran", cycle.ran);
		cycle_span.record("failed", cycle.failed);

		if inputs.once {
			return;
		}
		tokio::time::sleep(inputs.interval).await;
	}
}

struct CycleResult {
	inference: NodeReadiness,
	training: NodeReadiness,
	ran: usize,
	failed: usize,
}

async fn run_cycle(inputs: &PreflightInputs, metrics: &mut PreflightMetrics, labeler: &OptionalLabeler) -> CycleResult {
	let mut all_outcomes = Vec::new();
	let mut failed_checks = Vec::new();

	for check in &inputs.checks {
		for accel in &inputs.accelerators {
			if !check.applies_to(accel) {
				continue;
			}
			let span = tracing::info_span!(
				"preflight.check",
				check.name = check.name(),
				check.scope = check.scope().slug(),
				accel.vendor = accel.id.vendor.slug(),
				accel.model = %accel.model,
				accel.index = accel.id.drm_index as i64,
				status = field::Empty,
				message = field::Empty,
			);
			let (outcome, elapsed_ms) = run_one_check(check.as_ref(), accel).instrument(span).await;
			metrics.record(check.as_ref(), accel, &outcome, elapsed_ms);
			if matches!(outcome.status, Status::Fail | Status::Warn) {
				failed_checks.push(format!("{}@{}", check.name(), accel.id.drm_index));
			}
			all_outcomes.push(outcome.status);
		}
	}

	let inference = aggregate(&all_outcomes);
	// Today inference and training share the same outcome set; they'll
	// diverge once training-only checks (rccl/nccl loopback, ECC) land.
	let training = inference;

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

	CycleResult { inference, training, ran: all_outcomes.len(), failed: failed_checks.len() }
}

/// Run one (check, accelerator). The caller wraps this in a tracing
/// span so the OTel exporter sees the right parent/child relationship.
async fn run_one_check(check: &dyn PreflightCheck, accel: &Accelerator) -> (CheckOutcome, f64) {
	let started = Instant::now();
	let outcome = check.run(&CheckContext { accelerator: Some(accel) }).await;
	let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

	let span = tracing::Span::current();
	span.record("status", outcome.status.slug());
	if let Some(msg) = &outcome.message {
		span.record("message", msg.as_str());
	}

	(outcome, elapsed_ms)
}

fn now_rfc3339() -> String {
	use std::time::{SystemTime, UNIX_EPOCH};
	let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
	let (s, m, h) = (secs % 60, (secs / 60) % 60, (secs / 3600) % 24);
	let (y, mo, d) = days_to_ymd((secs / 86400) as i64);
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
