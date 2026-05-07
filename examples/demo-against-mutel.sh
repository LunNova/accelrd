#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 LunNova
# SPDX-License-Identifier: CC0-1.0
#
# Run accel-readiness once against a localhost mutel and print the
# discovered hardware + intended labels. Useful as a sanity check on a
# dev box before deploying as a DaemonSet.
#
# Assumes mutel is reachable at http://127.0.0.1:4318 (override with
# ACCEL_READINESS_OTLP_ENDPOINT). Pass extra args through, e.g.:
#   ./examples/demo-against-mutel.sh --rack rack-A --block block-1
set -euo pipefail

cd "$(dirname "$0")/.."

: "${ACCEL_READINESS_OTLP_ENDPOINT:=http://127.0.0.1:4318}"
: "${RUST_LOG:=info,opentelemetry=warn,reqwest=error}"
export ACCEL_READINESS_OTLP_ENDPOINT RUST_LOG

exec cargo run --quiet -- --once "$@"
