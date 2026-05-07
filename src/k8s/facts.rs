// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Best-effort node identity. Prefer `--node-name`, then `NODE_NAME`,
//! then `/proc/sys/kernel/hostname`.

use crate::config::Args;

pub fn node_name(args: &Args) -> String {
	if let Some(n) = &args.node_name {
		return n.clone();
	}
	if let Ok(n) = std::env::var("NODE_NAME")
		&& !n.is_empty()
	{
		return n;
	}
	std::fs::read_to_string("/proc/sys/kernel/hostname")
		.ok()
		.map(|s| s.trim().to_string())
		.filter(|s| !s.is_empty())
		.unwrap_or_else(|| "unknown".into())
}
