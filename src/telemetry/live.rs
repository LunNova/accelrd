// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Live telemetry loop. Every tick: ask each backend for a snapshot of
//! every accelerator it owns and emit the measurements as OTel gauges
//! attributed by accelerator identity.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::{KeyValue, global, metrics::Gauge};
use tokio::sync::RwLock;

use crate::sensors::{Accelerator, AcceleratorSensor};

pub async fn tick(
	backends: &[Box<dyn AcceleratorSensor>],
	accelerators: &[Accelerator],
	gauges: Arc<RwLock<GaugeCache>>,
) {
	let meter = global::meter("accel-readiness.live");
	for accel in accelerators {
		let backend = backends.iter().find(|b| b.vendor() == accel.id.vendor);
		let Some(backend) = backend else { continue };
		match backend.snapshot(accel).await {
			Ok(snapshot) => {
				for m in snapshot.measurements {
					let gauge = {
						let mut cache = gauges.write().await;
						cache.get_or_create(&meter, &m).clone()
					};
					let mut attrs = base_attrs(accel);
					for (k, v) in &m.attrs {
						attrs.push(KeyValue::new(k.to_string(), v.clone()));
					}
					gauge.record(m.value, &attrs);
				}
			}
			Err(e) => {
				tracing::warn!(accel = %accel.id, error = %e, "snapshot failed");
			}
		}
	}
}

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

/// OTel meters lazily-create gauge instruments per metric name. We cache
/// them so we don't re-create on every tick.
#[derive(Default)]
pub struct GaugeCache {
	gauges: HashMap<&'static str, Gauge<f64>>,
}

impl GaugeCache {
	pub fn get_or_create(
		&mut self,
		meter: &opentelemetry::metrics::Meter,
		m: &crate::sensors::Measurement,
	) -> &Gauge<f64> {
		self.gauges.entry(m.name).or_insert_with(|| {
			meter
				.f64_gauge(m.name.to_string())
				.with_unit(m.unit.to_string())
				.with_description(m.description.to_string())
				.build()
		})
	}
}

pub async fn run(
	backends: Vec<Box<dyn AcceleratorSensor>>,
	accelerators: Vec<Accelerator>,
	interval: Duration,
	once: bool,
) {
	let gauges = Arc::new(RwLock::new(GaugeCache::default()));
	loop {
		tick(&backends, &accelerators, gauges.clone()).await;
		if once {
			return;
		}
		tokio::time::sleep(interval).await;
	}
}
