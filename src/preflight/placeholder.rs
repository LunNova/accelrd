// SPDX-FileCopyrightText: 2026 LunNova
// SPDX-License-Identifier: CC0-1.0

//! Placeholder always-Pass check. Real checks land in follow-up work.

use async_trait::async_trait;

use super::{CheckContext, CheckOutcome, CheckScope, PreflightCheck, Status};

pub struct AlwaysPass;

#[async_trait]
impl PreflightCheck for AlwaysPass {
	fn name(&self) -> &'static str {
		"placeholder.always_pass"
	}

	fn scope(&self) -> CheckScope {
		CheckScope::SingleAccelerator
	}

	async fn run(&self, _ctx: &CheckContext<'_>) -> CheckOutcome {
		CheckOutcome {
			status: Status::Pass,
			message: Some("placeholder check; real preflights TBD".into()),
			measurements: Vec::new(),
		}
	}
}
