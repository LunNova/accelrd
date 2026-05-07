// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Optional Kubernetes integration. The daemon detects an in-cluster
//! service-account token and uses kube-rs to PATCH node labels. When
//! run outside a cluster (no SA token, or `--no-k8s`), the labeler
//! becomes a no-op and the daemon prints intended labels to logs.

pub mod facts;
pub mod labeler;

use std::path::Path;

use crate::config::Args;

/// "Are we running in a Kubernetes pod with a usable service account?"
/// Cheap, no network. Returns false on dev boxes.
pub fn looks_in_cluster() -> bool {
	const TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
	Path::new(TOKEN).exists() && std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
}

pub fn enabled(args: &Args) -> bool {
	!args.no_k8s && looks_in_cluster()
}
