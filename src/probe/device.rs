// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! RDMA device selection. Reads `/sys/class/infiniband/*` to find
//! kernel-registered IB/RoCE devices. Picking the first one when the
//! caller didn't pin a device matches what perftest itself does, but
//! we surface the resolved name in OTLP attributes so multi-device hosts
//! aren't ambiguous in dashboards.

use anyhow::{Context, anyhow};

const SYSFS_INFINIBAND: &str = "/sys/class/infiniband";

pub fn select(explicit: Option<&str>) -> anyhow::Result<String> {
	if let Some(d) = explicit {
		return Ok(d.into());
	}
	let mut found: Vec<String> = std::fs::read_dir(SYSFS_INFINIBAND)
		.with_context(|| format!("read {SYSFS_INFINIBAND}"))?
		.filter_map(|e| e.ok())
		.filter_map(|e| e.file_name().into_string().ok())
		.collect();
	found.sort();
	found
		.into_iter()
		.next()
		.ok_or_else(|| anyhow!("no RDMA devices in {SYSFS_INFINIBAND} (kernel ib drivers loaded?)"))
}
