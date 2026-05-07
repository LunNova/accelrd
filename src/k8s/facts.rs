// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Best-effort node identity. Prefer `--node-name`, then `NODE_NAME`,
//! then `/proc/sys/kernel/hostname`.

use crate::config::Resolved;

pub fn node_name(args: &Resolved) -> String {
	args.node_name
		.clone()
		.or_else(|| std::env::var("NODE_NAME").ok().filter(|s| !s.is_empty()))
		.or_else(|| {
			std::fs::read_to_string("/proc/sys/kernel/hostname")
				.ok()
				.map(|s| s.trim().to_string())
				.filter(|s| !s.is_empty())
		})
		.unwrap_or_else(|| "unknown".into())
}
