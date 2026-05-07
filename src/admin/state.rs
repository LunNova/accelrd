// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Shared application state for axum handlers. Cheaply cloneable —
//! kube::Client and reqwest::Client are both Arc-wrapped internally.

use std::sync::Arc;
use std::time::Instant;

use kube::Client;

use super::Resolved;
use super::mutel::MutelClient;

#[derive(Clone)]
pub struct AppState {
	pub kube: Option<Client>,
	pub mutel: MutelClient,
	pub started_at: Arc<Instant>,
}

impl AppState {
	pub async fn build(args: &Resolved) -> anyhow::Result<Self> {
		let kube = if args.no_k8s {
			None
		} else {
			match Client::try_default().await {
				Ok(c) => {
					tracing::info!("kube client initialized");
					Some(c)
				}
				Err(e) => {
					tracing::warn!(error = %e, "kube client init failed; cluster endpoints will report degraded");
					None
				}
			}
		};

		let mutel = MutelClient::new(args.mutel_endpoint.clone(), args.mutel_timeout_secs)?;

		Ok(Self {
			kube,
			mutel,
			started_at: Arc::new(Instant::now()),
		})
	}
}
