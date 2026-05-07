// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Live telemetry loop. Every tick: ask each backend for a snapshot of
//! every accelerator it owns and emit the measurements as OTel gauges
//! attributed by accelerator identity.

use std::time::Duration;

use opentelemetry::{KeyValue, global};

use crate::sensors::{Accelerator, AcceleratorSensor};
use crate::telemetry::{GaugeCache, base_attrs};

pub async fn run(
	backends: Vec<Box<dyn AcceleratorSensor>>,
	accelerators: Vec<Accelerator>,
	interval: Duration,
	once: bool,
) {
	let meter = global::meter("accelrd.live");
	let mut gauges = GaugeCache::default();
	loop {
		tick(&backends, &accelerators, &meter, &mut gauges).await;
		if once {
			return;
		}
		tokio::time::sleep(interval).await;
	}
}

async fn tick(
	backends: &[Box<dyn AcceleratorSensor>],
	accelerators: &[Accelerator],
	meter: &opentelemetry::metrics::Meter,
	gauges: &mut GaugeCache,
) {
	for accel in accelerators {
		let Some(backend) = backends.iter().find(|b| b.vendor() == accel.id.vendor) else {
			continue;
		};
		let snapshot = match backend.snapshot(accel).await {
			Ok(s) => s,
			Err(e) => {
				tracing::warn!(accel = %accel.id, error = %e, "snapshot failed");
				continue;
			}
		};
		let base = base_attrs(accel);
		for m in snapshot.measurements {
			let mut attrs = base.clone();
			attrs.extend(m.attrs.iter().map(|(k, v)| KeyValue::new(k.to_string(), v.clone())));
			gauges.get_or_create(meter, &m).record(m.value, &attrs);
		}
	}
}
