// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! OTLP exporter setup. Uses opentelemetry-otlp 0.31's HTTP/protobuf
//! transport so it works against any OTLP backend (mutel for local dev,
//! otelcol/tempo/jaeger in prod).

use std::collections::BTreeMap;
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

/// Build the OTel `Resource` for this process.
///
/// Uses `Resource::builder()` which seeds the SDK's default detectors:
/// `TelemetryResourceDetector` (telemetry.sdk.{name,language,version}),
/// `EnvResourceDetector` (parses `OTEL_RESOURCE_ATTRIBUTES` per spec,
/// percent-decoding included), and `SdkProvidedResourceDetector`
/// (service.name fallback). We layer our autodetected
/// service/host/os/process/k8s attributes on top via `.with_attributes()`.
///
/// In a K8s pod, the `k8s.*` block is populated from the SA namespace file
/// plus downward-API env vars set by the DaemonSet manifest. Outside a
/// pod, the `k8s.*` block is omitted.
fn build_resource(args: &Args, node_name: &str) -> Resource {
	let mut attrs: BTreeMap<String, String> = BTreeMap::new();

	// Service identity beyond what the SDK detectors handle. The
	// `SdkProvidedResourceDetector` only sets a fallback service.name;
	// we always set ours explicitly so it wins.
	attrs.insert("service.name".into(), args.service_name.clone());
	attrs.insert("service.version".into(), env!("CARGO_PKG_VERSION").into());

	// Host. `host.name` here means the *machine* hosting the process, which
	// in K8s is the Node — the daemon's `node_name` resolver already prefers
	// NODE_NAME (downward API) over the local hostname for that reason.
	attrs.insert("host.name".into(), node_name.into());
	attrs.insert("host.arch".into(), normalize_arch(std::env::consts::ARCH).into());
	attrs.insert("os.type".into(), std::env::consts::OS.into());
	if let Some(release) = read_trimmed("/proc/sys/kernel/osrelease") {
		attrs.insert("os.description".into(), release);
	}
	if let Some(id) = read_machine_id() {
		attrs.insert("host.id".into(), id);
	}

	// Process.
	attrs.insert("process.pid".into(), std::process::id().to_string());
	if let Ok(exe) = std::env::current_exe()
		&& let Some(name) = exe.file_name().and_then(|s| s.to_str())
	{
		attrs.insert("process.executable.name".into(), name.into());
	}

	// K8s — only when an SA token is mounted (the cheapest indicator of "we
	// are in a pod"). Pod UID, when available, doubles as service.instance.id.
	let pod_uid = if std::path::Path::new("/var/run/secrets/kubernetes.io/serviceaccount").exists() {
		populate_k8s_attrs(&mut attrs, node_name)
	} else {
		None
	};

	// service.instance.id should be unique per process instance for the
	// lifetime of that instance. Pod UID is ideal in K8s; outside, combine
	// boot_id (per-boot UUID) with PID so the value is stable across the
	// process's lifetime but doesn't alias across reboots.
	let instance_id = pod_uid.unwrap_or_else(|| match read_trimmed("/proc/sys/kernel/random/boot_id") {
		Some(b) => format!("{b}-{}", std::process::id()),
		None => std::process::id().to_string(),
	});
	attrs.insert("service.instance.id".into(), instance_id);

	let kvs: Vec<KeyValue> = attrs.into_iter().map(|(k, v)| KeyValue::new(k, v)).collect();
	Resource::builder().with_attributes(kvs).build()
}

/// Populate `k8s.*` attributes from the pod's downward API env + the SA
/// namespace file. Returns the pod UID (when available) so the caller
/// can adopt it as `service.instance.id`.
fn populate_k8s_attrs(attrs: &mut BTreeMap<String, String>, node_name: &str) -> Option<String> {
	if let Some(ns) = read_trimmed("/var/run/secrets/kubernetes.io/serviceaccount/namespace") {
		attrs.insert("k8s.namespace.name".into(), ns);
	}
	// k8s.node.name is just the resolved node identity — it's the one
	// thing that doesn't need new manifest plumbing.
	attrs.insert("k8s.node.name".into(), node_name.into());

	// Downward-API env vars (set by the DaemonSet manifest).
	let env_to_attr = &[
		("POD_NAME", "k8s.pod.name"),
		("POD_UID", "k8s.pod.uid"),
		("POD_NAMESPACE", "k8s.namespace.name"),
		("CONTAINER_NAME", "k8s.container.name"),
		("DAEMONSET_NAME", "k8s.daemonset.name"),
		("CLUSTER_NAME", "k8s.cluster.name"),
	];
	for (env_var, attr_key) in env_to_attr {
		if let Ok(v) = std::env::var(env_var)
			&& !v.is_empty()
		{
			attrs.insert((*attr_key).into(), v);
		}
	}

	// HOSTNAME-as-pod-name fallback. CRI-O/containerd typically set the
	// container's hostname to the pod name when POD_NAME isn't explicitly
	// passed; the SA-mount existence check upstream is sufficient evidence
	// we're in a pod.
	if !attrs.contains_key("k8s.pod.name")
		&& let Ok(h) = std::env::var("HOSTNAME")
		&& !h.is_empty()
	{
		attrs.insert("k8s.pod.name".into(), h);
	}

	attrs.get("k8s.pod.uid").cloned()
}

fn read_trimmed(path: &str) -> Option<String> {
	std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// /etc/machine-id is canonical on systemd hosts; /var/lib/dbus/machine-id
/// covers older non-systemd setups.
fn read_machine_id() -> Option<String> {
	for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
		if let Some(s) = read_trimmed(p) {
			return Some(s);
		}
	}
	None
}

/// Map Rust's `cfg(target_arch)` strings to OTel semconv `host.arch` values.
fn normalize_arch(rust_arch: &str) -> &str {
	match rust_arch {
		"x86_64" => "amd64",
		"aarch64" => "arm64",
		"arm" => "arm32",
		"powerpc" => "ppc32",
		"powerpc64" => "ppc64",
		other => other,
	}
}
