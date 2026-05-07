// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

pub mod live;
pub mod preflight;

use std::collections::HashMap;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Gauge, Meter};

use crate::sensors::{Accelerator, Measurement};

/// OTel meters lazily-create gauge instruments per metric name. The same
/// instrument can be re-used across recordings, so we cache them keyed by
/// metric name to avoid the rebuild cost on every tick.
#[derive(Default)]
pub struct GaugeCache {
	gauges: HashMap<&'static str, Gauge<f64>>,
}

impl GaugeCache {
	pub fn get_or_create(&mut self, meter: &Meter, m: &Measurement) -> &Gauge<f64> {
		self.gauges.entry(m.name).or_insert_with(|| {
			meter
				.f64_gauge(m.name.to_string())
				.with_unit(m.unit.to_string())
				.with_description(m.description.to_string())
				.build()
		})
	}
}

/// Standard per-accelerator attribute set every metric carries. Callers
/// `clone()` and append per-measurement attributes.
pub fn base_attrs(accel: &Accelerator) -> Vec<KeyValue> {
	vec![
		KeyValue::new("vendor", accel.id.vendor.slug()),
		KeyValue::new("model", accel.model.clone()),
		KeyValue::new("accel.index", accel.id.drm_index as i64),
		KeyValue::new("pci_addr", accel.id.pci_addr.clone()),
		KeyValue::new("memory_kind", accel.memory_kind.slug()),
		KeyValue::new("coverage", accel.coverage.slug()),
		KeyValue::new("fabric_domain", accel.fabric_domain.clone().unwrap_or_default()),
		KeyValue::new("numa_node", accel.numa_node.unwrap_or(-1) as i64),
	]
}
