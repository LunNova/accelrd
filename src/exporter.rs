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

const EXPORT_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Providers {
	pub tracer: opentelemetry_sdk::trace::SdkTracerProvider,
	pub meter: opentelemetry_sdk::metrics::SdkMeterProvider,
	pub logger: opentelemetry_sdk::logs::SdkLoggerProvider,
}

impl Providers {
	pub fn shutdown(self) {
		// Best-effort flush + shutdown. We log failures but don't
		// propagate — process exit happens regardless.
		log_shutdown("tracer", self.tracer.shutdown());
		log_shutdown("meter", self.meter.shutdown());
		log_shutdown("logger", self.logger.shutdown());
	}
}

fn log_shutdown<E: std::fmt::Display>(name: &str, result: Result<(), E>) {
	if let Err(e) = result {
		tracing::warn!(error = %e, "{name} shutdown");
	}
}

pub fn init(args: &Args, node_name: &str) -> anyhow::Result<Providers> {
	let endpoint = args.otlp_endpoint.trim_end_matches('/');
	let resource = build_resource(args, node_name);

	// Each signal builder (Span/Metric/Log) lives in a different module
	// with a typestate that diverges after `.with_http()`, so the macro
	// captures only the shared body — endpoint suffix, protocol, timeout,
	// build + Context.
	macro_rules! http_exporter {
		($builder:expr, $signal:literal) => {
			$builder
				.with_http()
				.with_endpoint(format!("{endpoint}/v1/{}", $signal))
				.with_protocol(Protocol::HttpBinary)
				.with_timeout(EXPORT_TIMEOUT)
				.build()
				.with_context(|| concat!("build ", $signal, " exporter"))?
		};
	}

	let tracer = opentelemetry_sdk::trace::SdkTracerProvider::builder()
		.with_batch_exporter(http_exporter!(opentelemetry_otlp::SpanExporter::builder(), "traces"))
		.with_resource(resource.clone())
		.build();
	global::set_tracer_provider(tracer.clone());

	let meter = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
		.with_periodic_exporter(http_exporter!(opentelemetry_otlp::MetricExporter::builder(), "metrics"))
		.with_resource(resource.clone())
		.build();
	global::set_meter_provider(meter.clone());

	let logger = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
		.with_batch_exporter(http_exporter!(opentelemetry_otlp::LogExporter::builder(), "logs"))
		.with_resource(resource)
		.build();

	Ok(Providers { tracer, meter, logger })
}

fn build_resource(args: &Args, node_name: &str) -> Resource {
	Resource::builder_empty()
		.with_attributes([
			KeyValue::new("service.name", args.service_name.clone()),
			KeyValue::new("service.version", env!("CARGO_PKG_VERSION").to_string()),
			KeyValue::new("host.name", node_name.to_string()),
			KeyValue::new("host.arch", std::env::consts::ARCH.to_string()),
			KeyValue::new("os.type", std::env::consts::OS.to_string()),
			KeyValue::new("telemetry.sdk.name", "opentelemetry"),
			KeyValue::new("telemetry.sdk.language", "rust"),
		])
		.build()
}
