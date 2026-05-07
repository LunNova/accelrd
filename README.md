# accel-readiness

Vendor-neutral GPU/accelerator readiness daemon. Designed to run as a
Kubernetes DaemonSet on accelerator nodes, where it:

- Reads accelerator state via **kernel sysfs and /proc only** — no NVML,
  no ROCm, no shelling out to `nvidia-smi` / `rocm-smi`.
- Emits **OTLP metrics, logs, and traces** to any OTLP/HTTP backend.
- Discovers per-node topology (NUMA, intra-node fabric domains, partitioning)
  and publishes hierarchical labels for topology-aware schedulers
  (Kueue TAS, scheduler-plugins, Volcano).
- Runs a pluggable preflight check matrix. Real checks (rccl loopback,
  ECC, fabric integrity, etc.) are out of scope of this initial cut —
  the trait + dispatch + Skipped semantics are the deliverable.

Sibling project to [mutel](../mutel), which is the OTLP backend used during
local development. Any OTLP backend works in production.

## Build

```sh
cargo build --release
```

No vendor SDKs to satisfy — the binary statically resolves and runs on
any Linux box with a populated `/sys/class/drm`. Tested on NixOS 26.05
with kernel 6.18.

## Run locally (against a localhost mutel)

```sh
cargo run -- --otlp-endpoint http://127.0.0.1:4318 --rack dev-rack --block dev-block
```

`--once` runs a single cycle and exits — useful in tests or one-shot
debugging:

```sh
cargo run -- --once --otlp-endpoint http://127.0.0.1:4318
```

When run outside Kubernetes (no service-account token at
`/var/run/secrets/kubernetes.io/serviceaccount/token`), the labeler
auto-disables and prints intended labels at info level on the first
reconcile so you can preview what the daemon would do in-cluster.

## What's emitted (per accelerator)

| Metric | Unit | Meaning |
|---|---|---|
| `accel.memory.vram.total` | By | Dedicated VRAM size from amdgpu sysfs (small on iGPUs) |
| `accel.memory.vram.used` | By | Used VRAM bytes |
| `accel.memory.gtt.total` | By | Host aperture — UMA carrier on iGPUs, ~31 GiB on Raphael |
| `accel.memory.gtt.used` | By | Used GTT bytes |
| `accel.memory.dedicated.total` | By | Nvidia: derived from PCI BAR1 |
| `accel.utilization` | 1 | AMD `gpu_busy_percent` / 100 (AMD only) |
| `accel.temperature` | Cel | hwmon temp1_input |
| `accel.power.usage` | W | hwmon power1_average where available |
| `accel.sensor.health` | 1 | 1 = sensor reads succeeding, 0 = backend broken |
| `accel.preflight.check.duration_ms` | ms | Per-check wall clock |
| `accel.preflight.check.pass` | 1 | 1 if Pass, 0 for Warn/Fail; not emitted for Skipped |

Every metric is attributed by `vendor`, `model`, `accel.index`,
`pci_addr`, `memory_kind`, `coverage`, `fabric_domain`, `numa_node`.

A preflight cycle opens a parent span `preflight_cycle` and one child
span per `(check, accelerator)` pair. A summary log line lands at the
end of each cycle.

## Labels published to the node

`accel-topo.lunnova.dev/{block,rack,fabric-domain.<id>,fabric-domains-count}`,
`accel.lunnova.dev/{vendor,model,memory-kind}.<key>.count`,
`accel.lunnova.dev/{vendor.<v>.physical-count,total-count}`,
`accel-ready.lunnova.dev/{inference,training}`.

Annotations: `accel.lunnova.dev/inventory` (full per-card JSON) and
`accel.lunnova.dev/fabric-graph` (fabric-domain → member set).

See [the design plan](../../.claude/plans/let-s-plan-a-ui-effervescent-floyd.md)
for the full schema and rationale, including the heterogeneous /
multi-fabric / partitioned-card scenarios it's designed to handle.

## Coverage caveats

**Nvidia is identification-only without NVML.** Sysfs gives us model
name, driver version, BAR-derived dedicated VRAM total, and basic
placement metadata (NUMA, local CPUs). Live utilization, free memory,
temperature, and power are *not* available without linking
libnvidia-ml.so. We surface this via `coverage="identification-only"`
on the metric attributes, and the preflight harness will return
`Skipped` for runtime-property checks against Nvidia accelerators.

**NVLink topology is unreachable from sysfs.** Each Nvidia card is
treated as its own single-member fabric domain. xGMI on AMD Instinct is
fully sysfs-discoverable.

These are deliberate trade-offs. A feature-gated NVML backend is a
follow-up if richer Nvidia coverage becomes load-bearing.

## Status

- ✅ AMD sensor (sysfs, including SR-IOV capacity surfacing).
- ✅ Nvidia sensor (sysfs / proc).
- ✅ Intel sensor (i915 / Xe sysfs).
- ✅ Multi-vendor + multi-card + partitioning detection (DRM-share + SR-IOV numvfs).
- ✅ UMA detection (GTT-vs-VRAM heuristic).
- ✅ Topology discovery (NUMA + xGMI; PCIe fallback).
- ✅ OTLP exporter (metrics, logs, traces over HTTP/protobuf).
- ✅ Hierarchical label generation (Kueue-TAS-compatible).
- ✅ Preflight trait + placeholder + Skipped semantics.
- ✅ Sysfs-friendly preflight checks: sensor.readable, drm.render_node_present,
  driver.loaded, temperature.below_throttle, memory.floor.
- ✅ K8s labeler (auto-disabled outside cluster).
- ✅ K8s labeler in-cluster PATCH path (RFC 7396 merge, retry/backoff, stale-key removal).
- ✅ Manifests (DaemonSet YAML + RBAC + namespace, sysfs/proc hostPath read-only).
- ✅ Nix flake (rust-overlay + pre-commit hooks mirroring mutel's setup).
- ⏳ Vendor-runtime preflight checks (rccl/nccl loopback, ECC scrub, firmware
  version) — these need vendor SDKs or a sidecar container, deferred to a
  separate runtime-checks module.
- ⏳ DRA `ResourceSlice` driver — separate workstream.
- ⏳ LLDP-driven rack discovery — needs lldpd integration; config-fed today.
- ⏳ Cross-node fabric-domain reconciliation — cluster-side concern.
