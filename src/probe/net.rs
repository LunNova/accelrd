// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! TCP-connect backoff used by client-mode probes to wait until the
//! server-side `ib_send_bw` has bound its QP-exchange port. Both pods
//! get created near-simultaneously by the prober, so the client always
//! loses the race; spinning a TCP probe is cheaper than provisioning a
//! shared barrier.

use std::time::{Duration, Instant};

use anyhow::bail;

pub async fn wait_connectable(host: &str, port: u16, deadline: Instant) -> anyhow::Result<()> {
	let mut backoff = Duration::from_millis(200);
	let cap = Duration::from_secs(2);
	loop {
		if Instant::now() >= deadline {
			bail!("timeout waiting for {host}:{port} to accept TCP");
		}
		match tokio::net::TcpStream::connect(format!("{host}:{port}")).await {
			Ok(_) => return Ok(()),
			Err(_) => {
				tokio::time::sleep(backoff).await;
				backoff = (backoff * 2).min(cap);
			}
		}
	}
}
