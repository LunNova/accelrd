// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! OTLP exporter setup. Uses opentelemetry-otlp 0.31's HTTP/protobuf
//! transport so it works against any OTLP backend (mutel for local dev,
//! otelcol/tempo/jaeger in prod).

use std::time::Duration;

use anyhow::Context;
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::Resource;

use crate::config::Args;

pub struct Providers {
	pub tracer: opentelemetry_sdk::trace::SdkTracerProvider,
	pub meter: opentelemetry_sdk::metrics::SdkMeterProvider,
	pub logger: opentelemetry_sdk::logs::SdkLoggerProvider,
}

impl Providers {
	pub fn shutdown(self) {
		// Best-effort flush + shutdown. We log failures but don't
		// propagate — process exit happens regardless.
		if let Err(e) = self.tracer.shutdown() {
			tracing::warn!(error = %e, "tracer shutdown");
		}
		if let Err(e) = self.meter.shutdown() {
			tracing::warn!(error = %e, "meter shutdown");
		}
		if let Err(e) = self.logger.shutdown() {
			tracing::warn!(error = %e, "logger shutdown");
		}
	}
}

pub fn init(args: &Args, node_name: &str) -> anyhow::Result<Providers> {
	let endpoint = args.otlp_endpoint.trim_end_matches('/');
	let resource = Resource::builder_empty()
		.with_attributes([
			KeyValue::new("service.name", args.service_name.clone()),
			KeyValue::new("service.version", env!("CARGO_PKG_VERSION").to_string()),
			KeyValue::new("host.name", node_name.to_string()),
			KeyValue::new("host.arch", std::env::consts::ARCH.to_string()),
			KeyValue::new("os.type", std::env::consts::OS.to_string()),
			KeyValue::new("telemetry.sdk.name", "opentelemetry"),
			KeyValue::new("telemetry.sdk.language", "rust"),
		])
		.build();

	let timeout = Duration::from_secs(3);

	let span_exporter = opentelemetry_otlp::SpanExporter::builder()
		.with_http()
		.with_endpoint(format!("{endpoint}/v1/traces"))
		.with_protocol(Protocol::HttpBinary)
		.with_timeout(timeout)
		.build()
		.context("build span exporter")?;
	let tracer = opentelemetry_sdk::trace::SdkTracerProvider::builder()
		.with_batch_exporter(span_exporter)
		.with_resource(resource.clone())
		.build();
	global::set_tracer_provider(tracer.clone());

	let metric_exporter = opentelemetry_otlp::MetricExporter::builder()
		.with_http()
		.with_endpoint(format!("{endpoint}/v1/metrics"))
		.with_protocol(Protocol::HttpBinary)
		.with_timeout(timeout)
		.build()
		.context("build metric exporter")?;
	let meter = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
		.with_periodic_exporter(metric_exporter)
		.with_resource(resource.clone())
		.build();
	global::set_meter_provider(meter.clone());

	let log_exporter = opentelemetry_otlp::LogExporter::builder()
		.with_http()
		.with_endpoint(format!("{endpoint}/v1/logs"))
		.with_protocol(Protocol::HttpBinary)
		.with_timeout(timeout)
		.build()
		.context("build log exporter")?;
	let logger = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
		.with_batch_exporter(log_exporter)
		.with_resource(resource)
		.build();

	Ok(Providers { tracer, meter, logger })
}
