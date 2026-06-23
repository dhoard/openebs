---
oep-number: OEP 4185
title: DiskPool Disk Detach, I/O Errors and I/O Stall Handling
authors:
  - @tiagolobocastro
  - @abhilashshetty04
owners:
  - @tiagolobocastro
  - @abhilashshetty04
editor: TBD
creation-date: 2026-02-24
last-updated: 2026-06-23
status: implemented
---

# DiskPool Disk Detach, I/O Errors and I/O Stall Handling

## Table of Contents

- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Terminology](#terminology)
- [Proposal](#proposal)
  - [User Stories](#user-stories)
  - [Implementation Details](#implementation-details)
    - [Disk Failure Types](#disk-failure-types)
    - [DiskPool URI Backends](#diskpool-uri-backends)
    - [bdev Hot-Removal](#bdev-hot-removal)
    - [Pool Status and Reasons](#pool-status-and-reasons)
    - [Pool Probe API](#pool-probe-api)
    - [I/O Error Tracking and Alerts](#io-error-tracking-and-alerts)
    - [I/O Stall Detection](#io-stall-detection)
      - [What is a stall?](#what-is-a-stall)
      - [Detection mechanism](#detection-mechanism)
      - [Where the timeout is registered](#where-the-timeout-is-registered)
      - [In-memory pool health cache](#in-memory-pool-health-cache)
      - [Stall → Faulted transition](#stall--faulted-transition)
      - [Recovery: Faulted → Online](#recovery-faulted--online)
      - [Flakiness detection (sliding window)](#flakiness-detection-sliding-window)
      - [Local child stall handling](#local-child-stall-handling)
    - [Control-Plane Scheduling Integration](#control-plane-scheduling-integration)
    - [Import Backoff](#import-backoff)
    - [io-engine gRPC API Changes](#io-engine-grpc-api-changes)
    - [Control-Plane gRPC API Changes](#control-plane-grpc-api-changes)
    - [RESTful OpenAPI Changes](#restful-openapi-changes)
    - [Custom Resource Definition Changes](#custom-resource-definition-changes)
    - [Helm Tunables](#helm-tunables)
  - [Risks and Mitigations](#risks-and-mitigations)
- [Drawbacks](#drawbacks)
- [Alternatives](#alternatives)
- [Testing](#testing)

## Summary

This proposal describes how OpenEBS Mayastor should detect and respond to DiskPool backing-disk
failures, hot-removals, persistent I/O errors, and I/O stalls. Today, when a backing device is
removed or fails, the pool continues to show as `Online` while I/O silently fails and the
control-plane enters a loop of futile replica create/delete and rebuild attempts. This OEP
introduces a multi-layered response: automatic bdev eviction on hot-removal, a new pool-probe gRPC
API for pre/post import health checks, enriched pool status with structured reasons, an I/O error
alert hierarchy, and I/O stall detection — all surfaced through updated gRPC, REST, and CRD APIs
and driven by configurable Helm tunables.

## Motivation

When the backing device of a DiskPool is fully removed (hot-detach) or fails hard:

1. The pool continues to report `Online` while every submitted I/O fails.
2. The control-plane cannot distinguish "node is gone" from "disk is gone", so it keeps retrying
   pool imports and replica operations, flooding logs and wasting cluster resources.
3. Rebuilds that depend on the failed replica are triggered in a loop even though they cannot
   succeed while the source disk is bad.
4. Users have no actionable information: they see `Unknown` or a blank status with no reason code.

Softer failures (intermittent errors, bad sectors, transient stalls) are equally invisible today.
Catching them early lets the scheduler prefer healthy pools before data loss or prolonged
unavailability forces a harder intervention.

### Goals

- Automatically unload a DiskPool (unregister the bdev) when the backing device is hot-removed.
- Introduce a `PoolProbe` gRPC RPC so the control-plane can verify device presence and basic I/O
  health before and after an import attempt, without flooding logs on every retry.
- Enrich pool status with structured reason codes (`DiskNotFound`, `DiskReadIoError`,
  `ForeignPoolName`, `InvalidSuperBlock`, etc.) so users and operators can act on concrete
  information.
- Distinguish `Offline` (device gone) from `Faulted` (I/O errors) from `Unknown` (node
  unreachable), eliminating the current overuse of `Unknown`.
- Track runtime I/O errors per pool and per disk, exposing them through gRPC, REST, and CRD.
- Raise structured alerts (`Attention` → `Warning` → `Critical`) based on configurable thresholds
  for error count and I/O stall transitions.
- Feed alert status into the control-plane scheduler so it prefers healthy pools (`Warning`) or
  completely avoids broken ones (`Critical`).
- Detect I/O stalls (queue frozen beyond a deadline) and raise a `Critical` alert; attempt bdev
  reset to recover.
- Apply exponential backoff to pool import retries when probes or prior imports have returned
  structured errors.
- Expose a `ClearErrors` gRPC operation and a matching REST endpoint so operators can
  acknowledge/reset error state after remediation.
- Surface all of the above in the `DiskPoolStatus` CRD so kubectl and the operator UI reflect
  accurate health.

### Non-Goals

- SMART / NVMe telemetry integration (tracked separately).
- RAID-based pool redundancy (pools currently use a single disk).
- Predictive failure analysis based on error trends (future work, out of scope for this OEP).
- Detailed per-sector bad-block mapping.
- Automated replica migration triggered by disk health.
- vfio-pci stall/probe behaviour (marked TODO; aio and uring are in scope for this OEP).

## Terminology

| Term | Definition |
|------|-----------|
| **OpenEBS** | Open-source, Kubernetes-native storage solution providing persistent storage for containerised applications. |
| **Mayastor** | Cloud-native storage engine with replication capabilities, part of the OpenEBS project. |
| **DiskPool** | Storage abstraction that aggregates block devices into a pool from which Mayastor provisions persistent volumes. |
| **SPDK** | Storage Performance Development Kit — open-source user-space storage libraries. |
| **bdev** | SPDK block-device abstraction providing a unified interface over NVMe, aio, uring, malloc, etc. |
| **aio** | Linux Asynchronous I/O — file-descriptor-based async I/O (also used for raw block devices). |
| **uring** | `io_uring` — modern Linux async I/O API with lower per-operation overhead than aio. |
| **vfio-pci** | VFIO binding for PCIe devices; enables SPDK to bypass the kernel storage stack. |
| **bdev hot-removal** | The act of automatically unregistering an SPDK bdev in response to a device disappearing. |
| **Pool probe** | A lightweight, non-destructive health check on a disk before or after a pool import attempt. |
| **Alert status** | A four-level classification (`Healthy`, `Attention`, `Warning`, `Critical`) derived from runtime metrics. |
| **I/O stall** | A condition where pool I/O queue has not made progress beyond a configurable deadline. |

## Proposal

### User Stories

#### Story 1 — Hot-removed disk reported clearly

As a cluster administrator, when a disk backing a DiskPool is physically removed while the cluster
is running, I want the DiskPool to automatically transition to `Offline` (not stay `Online`)
within a short time, with a reason of `DeviceNotPresent`, so I know exactly which pool is affected
and why — without having to grep through logs.

#### Story 2 — Degraded disk surfaces actionable alerts

As a storage operator, when a disk starts returning intermittent I/O errors, I want the pool to
expose an escalating alert status (`Attention` → `Warning`) so that the scheduler stops placing new
replicas on the suspect pool before data-path errors propagate further, while I investigate whether
the disk needs replacement.

#### Story 3 — Stalled I/O auto-recovers or escalates

As a platform engineer, when pool I/O stalls (queue frozen), I want the system to attempt an
automatic bdev reset to recover the device. If the stall persists, I want a `Critical` alert raised
and the pool excluded from scheduling until the condition is cleared, preventing cascading rebuild
loops.

#### Story 4 — Import failure reason is visible in kubectl

As a developer troubleshooting a new cluster, when a DiskPool fails to import because the device
path is wrong or the superblock CRC is corrupt, I want the DiskPool CR's `conditions` field to
contain a `PoolReady: False` condition with a human-readable reason (e.g., `DiskNotFound`,
`InvalidSuperBlock`) so I can fix the root cause immediately without reading data-plane logs.

#### Story 5 — Operator can acknowledge and clear error state

As an operator, after replacing a faulty disk and reattaching it, I want to clear the accumulated
error counters and warnings so the pool transitions back to `Healthy` without requiring a full
restart or pool recreate.

### Implementation Details

#### Disk Failure Types

Real-world disk failures are rarely binary. The design must accommodate:

| Failure Mode | Symptom | Handling |
|---|---|---|
| Physical hot-removal / full device loss | File descriptor becomes invalid; all I/O returns `ENODEV`/`EIO` | bdev auto-unregistration → pool unload → `Offline` |
| Bad sectors (localised corruption) | Reads at specific LBAs fail with `EIO`; other I/O succeeds | Error counter increment; alert escalation |
| Intermittent read/write errors | Sector sometimes succeeds, sometimes fails; long latency spikes | Error counter + stall transition tracking |
| Pending sector reallocation exhaustion | Firmware remap pool empty; hard failures on new bad sectors | Error counter; operator notified via `Warning` |
| Controller / firmware glitch | Random `EIO` across different offsets | Error counter |
| Interface / cabling issue | Transient communication failures | Error counter + stall transitions |
| Thermal / mechanical stress | Occasional mis-seeks; head crash on spinning media | Error counter |
| Power instability (SSDs) | Incomplete writes during erase/program cycle | Error counter; `InvalidSuperBlock` on next import |

> **Note:** This OEP focuses on error counting and alerting. SMART-based predictive analysis is
> tracked separately (GTM-4216).

#### DiskPool URI Backends

A DiskPool may be backed by:

- `aio://` — opens a file descriptor at bdev registration time; reused by all I/O channels.
- `uring://` — same fd-based model via `io_uring`.
- `vfio-pci://` — bypasses kernel; behaviour on device removal is TBD (out of scope).

For `aio` and `uring`, when the device is detached the fd becomes invalid and all subsequent I/O
fails. The bdev layer will receive error callbacks, triggering the hot-removal path described below.

#### bdev Hot-Removal

When the backing device disappears:

1. SPDK delivers an error event (or I/O completions with terminal error codes) to the bdev layer.
2. io-engine registers a hot-removal callback per bdev. On trigger, it:
   a. Unregisters the bdev (drains in-flight I/O, cancels pending I/O with errors).
   b. Unloads the associated DiskPool (equivalent to a forced destroy without data loss).
3. The pool transitions from `Online` to an unloaded state. The control-plane, on its next
   reconcile loop, will discover the pool is missing and — instead of immediately retrying import —
   will run a `PoolProbe` to determine whether the device is present.

> Without this step, the pool stays `Online` indefinitely and the fd yields `EIO` for every I/O
> submitted — the current broken behaviour.

#### Pool Status and Reasons

The existing pool status vocabulary is extended:

| Status | Reason | Meaning |
|--------|--------|---------|
| `Online` | — | Normal operation. |
| `Suspected` | `IoError`, `IoStallIntermittent` | Pool has non-critical alerts; still usable but scheduler deprioritises it. |
| `Degraded` | N/A (requires RAID; not applicable to single-disk pools) | — |
| `Faulted` | `DiskReadIoError`, `InvalidSuperBlock`, `ForeignPoolUid`, … | Pool is inaccessible due to a device or metadata error. |
| `Offline` | `DiskNotFound`, `DeviceNotPresent` | Device is gone. Node is reachable. |
| `Unknown` | — | Node itself is unreachable or no information available. Reserved for node-level issues. |

**Decision**: `DeviceNotPresent` maps to `Offline`; `DiskReadIoError` maps to `Faulted`. `Unknown`
is no longer used for device-level failures.

#### Pool Probe API

A new lightweight gRPC RPC is introduced on the `PoolRpc` service:

```protobuf
message PoolProbeRequest {
  PoolImportRequest request = 1;
  // Issue a read I/O and verify success within a short timeout.
  bool probe_reads = 2;
  // Read the superblock and validate its integrity.
  bool validate_sb = 3;
}

message PoolProbeResponse {
  PoolHealth health = 1;
}

message MultiPoolProbeRequest {
  repeated PoolProbeRequest requests = 1;
}

message MultiPoolProbeResponse {
  repeated PoolProbeResponse responses = 1;
}

service PoolRpc {
  rpc PoolProbe(PoolProbeRequest) returns (PoolProbeResponse) {}
  rpc MultiPoolProbe(MultiPoolProbeRequest) returns (MultiPoolProbeResponse) {}
}

message PoolHealth {
  string name = 1;
  string uuid = 2;
  repeated DiskImportError import_errors = 3;
}

message DiskImportError {
  string disk = 1;
  ProbeError error = 2;
}

enum ProbeErrorCode {
  ProbeUnknown        = 1;
  DiskNotFound        = 2;
  DiskReadIoError     = 3;
  ForeignPoolName     = 4;
  ForeignPoolUid      = 5;
  SuperBlock          = 6;
  InvalidSuperBlock   = 7;
}

message ProbeError {
  ProbeErrorCode code = 1;
  string msg = 2;  // human-readable details
}
```

**Design invariant**: the probe must never return a gRPC error. It always returns `PoolProbeResponse`
with populated `import_errors` on failure. This allows the control-plane to probe multiple pools in
a single call (`MultiPoolProbe`) without one bad pool aborting the batch.

The in-process Rust implementation follows a `BdevProbe` trait:

```rust
trait BdevProbe {
    async fn probe(&self) -> Probe;
}

impl BdevProbe for Aio {
    async fn probe(&self) -> Probe {
        Probe {
            device_exists: std::fs::exists(&self.name).ok(),
            read_ok: None,
            sb_ok: None,
        }
    }
}
```

The control-plane calls `PoolProbe` (with `probe_reads: true`) whenever a pool import fails, and
records the result in `PoolDiag.import_errors`. This replaces the current behaviour of retrying
blind imports in a tight loop.

#### I/O Error Tracking and Alerts

The SPDK bdev layer already exposes per-bdev error statistics via `spdk_bdev_io_error_stat`:

```c
struct spdk_bdev_io_error_stat {
    uint32_t error_status[-SPDK_MIN_BDEV_IO_STATUS];
};
```

io-engine will:

1. Sample these counters on a configurable interval (or on I/O completion callbacks).
2. Accumulate a monotonic `io_error_count` per pool disk.
3. Compare the running count against a configurable `error_threshold`.
4. Raise or lower the `PoolAlertStatus` accordingly.

**Alert escalation model**:

| Condition | Alert Status | Pool State Impact |
|-----------|-------------|-------------------|
| No errors | `Healthy` | `Online` |
| `io_error_count` > 0, ≤ threshold | `Attention` | `Online` (scheduler notices) |
| `io_error_count` > threshold **or** `stall_transition_count` > `stall_threshold` within window | `Warning` | `Suspected` (scheduler deprioritises) |
| I/O queue stalled beyond `stall_deadline` | `Critical` | `Suspected`; scheduler excludes pool |

> **Note**: Reaching the error threshold alone does not immediately take the pool offline. Some I/O
> may still succeed (bad sectors, temperature oscillation, firmware bugs). The pool is cordoned
> from new replica placement but remains functional for existing replicas. A future enhancement
> can add a `Faulted` override for operators who want a stricter policy (see `ClearErrors` below).

Per-pool error state exposed via gRPC:

```protobuf
message PoolErrors {
  PoolAlerts alerts = 1;
  uint64 io_error_count = 2;
  uint64 io_error_threshold = 3;
  bool io_stalled = 4;
  uint64 stall_transition_count = 5;
  uint64 stall_threshold = 6;
}

message PoolAlerts {
  optional PoolAlertStatus status = 1;
  repeated PoolAlert notice = 2;      // informational; no state change
  repeated PoolAlert attention = 3;
  repeated PoolAlert warning = 4;
  repeated PoolAlert critical = 5;
}

enum PoolAlertStatus {
  Healthy   = 0;
  Attention = 1;
  Warning   = 2;
  Critical  = 3;
}

enum PoolAlert {
  IoStalled,
  IoStallIntermittent,
  IoStallIntermittentExc,
  IoError,
  IoErrorExc,
}
```

#### I/O Stall Detection

##### What is a stall?

A stall is a condition where I/O is submitted to the backing device but never completes — it is
not failed with an error, it simply makes no progress. This is distinct from I/O errors (which
return `EIO` promptly) and from hot-removal (where the fd becomes invalid).

The canonical trigger is a broken network path to a remotely-attached disk
(NVMe-oF, iSCSI, etc.) where:

- The host still shows the device in its device tree.
- The file descriptor is still valid.
- SPDK submits I/O successfully to the kernel.
- The kernel initiator queues the I/O but cannot deliver it because the path is down.

Unlike a hot-removal scenario, no bdev error event is delivered; the I/O silently occupies a
slot in the kernel queue forever.

**Impact on Pool I/O**: Every pool gRPC operation that requires I/O holds the Pool service lock.
Once a stall occurs that lock is never released, causing all subsequent gRPC calls to time out
(the control-plane enforces a 5 s gRPC deadline). The pool appears `Online` from the outside.

**Impact on local child I/O**: When a volume nexus has a local child whose pool is stalled, the
child's I/O also hangs. The child remains `Online` (no error is returned), so the volume does not
immediately degrade. This is the most dangerous case because no automatic recovery signal arrives.

##### Detection mechanism

SPDK provides a per-descriptor timeout:

```c
int spdk_bdev_set_timeout(
    struct spdk_bdev_desc  *desc,
    uint64_t                timeout_in_sec,
    spdk_bdev_io_timeout_cb cb_fn,
    void                   *cb_arg);

typedef void (*spdk_bdev_io_timeout_cb)(void *cb_arg, struct spdk_bdev_io *bdev_io);
```

The poller is registered per I/O channel and fires the callback for any in-flight I/O that has
not completed within `timeout_in_sec`.

**Key constraint**: for `aio` and `uring`, SPDK abort I/O is not a supported I/O type, and a bdev
reset cannot cancel I/O that is already stuck in the kernel initiator. The callback is therefore
a **notification only** — it triggers state transitions, not direct I/O cancellation.

##### Where the timeout is registered

Two descriptors need a timeout:

| Descriptor | Who registers it | What it catches |
|---|---|---|
| `bs_dev` desc (blobstore device) | Pool create/import path | All lvs/blob I/O for the pool. Does not identify which individual lvol timed out — acceptable for pool-level health. |
| Local child bdev desc | Nexus child add path | I/O on a specific local child. Remote replicas are handled by the nvmx layer and do not need this. |

A new SPDK helper exposes the blobstore-level descriptor from the Rust side:

```c
/* Sets (or updates) the I/O timeout on the bdev desc backing a blobstore device. */
int spdk_bdev_update_bs_desc_timeout(
    struct spdk_bs_dev     *bs_dev,
    uint64_t                timeout_in_sec,
    spdk_bdev_io_timeout_cb cb_fn,
    void                   *cb_arg);
```

Rust registration after create or import success:

```rust
struct TimeoutCtx {
    pool_name: String,
}

let lvs = pool.as_inner_ptr();
let ctx = Box::new(TimeoutCtx { pool_name: args.name.clone() });
let ctx_ptr = Box::into_raw(ctx) as *mut std::ffi::c_void;
let rc = unsafe {
    spdk_bdev_update_bs_desc_timeout(
        (*lvs).bs_dev,
        stall_deadline_secs,
        Some(Self::bs_io_timeout_cb),
        ctx_ptr,
    )
};
assert_eq!(rc, 0, "non-zero errno setting pool timeout after import");

extern "C" fn bs_io_timeout_cb(ctx: *mut c_void, _bdev_io: *mut spdk_bdev_io) {
    let timeout_ctx = unsafe { &mut *(ctx as *mut TimeoutCtx) };
    info!("bdev io timed out on pool={}", timeout_ctx.pool_name);
    fault_pool_health(&timeout_ctx.pool_name, Some(PoolStateReason::IOStall));
}
```

##### In-memory pool health cache

A node-local, in-memory cache tracks health and stall-transition history for every pool:

```rust
pub static POOL_HEALTH_CACHE: Lazy<Arc<RwLock<HashMap<String, PoolHealth>>>> =
    Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));

pub struct PoolHealth {
    pub state: PoolState,
    pub transition_timestamps: Vec<Instant>,
}

pub enum PoolState {
    Normal,
    IOStall,
}
```

- An entry is **inserted** on pool create or import.
- An entry is **removed** on pool export or destroy.
- `list_pool` reads this cache to populate `PoolErrors` in the gRPC response.
- `RwLock` ensures concurrent reads are safe while writes are serialised.

##### Stall → Faulted transition

When `bs_io_timeout_cb` fires:

1. `POOL_HEALTH_CACHE` is updated: `state = IOStall`.
2. `Instant::now()` is appended to `transition_timestamps`.
3. `PoolAlertStatus` is promoted to `Critical`; `IoStalled` is added to the `critical` alert list.
4. Pool state is set to `POOL_SUSPECTED` (control-plane stops placing new replicas).

##### Recovery: Faulted → Online

Timeout on a network-attached path is often transient (brief path glitch, switch failover). When
the path re-establishes, pending kernel I/O completes. To detect recovery:

1. From the timeout callback, a background task issues a **superblock read probe** on the same bdev.
2. If the superblock read succeeds:
   - A bdev reset is issued to flush any remaining pending I/O.
   - `PoolState` transitions to `Normal`; `IoStalled` is removed from `critical`.
   - `IoStallIntermittent` is added to `warning`; `stall_transition_count` is incremented.
3. If the path never recovers within the NVMe-oF `controller_loss_tmo`, the NVMe-oF controller is
   torn down and the device is detached; the bdev hot-removal path takes over from that point.

##### Flakiness detection (sliding window)

Pools that oscillate between stalled and online are a reliability risk even when currently
`Online`. The transition history is tracked in a sliding window:

```rust
fn update_transition_timestamps(&mut self) {
    self.transition_timestamps
        .retain(|ts| ts.elapsed() < Duration::from_secs(stall_transition_window_secs));
}
```

On every `list_pool` call, expired timestamps are pruned; the remaining count is
`stall_transition_count` (stall events within the rolling window).

| `stall_transition_count` vs threshold | Alert raised | Pool state |
|---|---|---|
| 0, no active stall | `Healthy` (if no other alerts) | `Online` |
| > 0, ≤ `stall_transition_threshold` | `IoStallIntermittent` → `Attention` | `Online` |
| > `stall_transition_threshold` | `IoStallIntermittentExc` → `Warning` | `Suspected` |
| Active stall (`IOStall` state) | `IoStalled` → `Critical` | `Suspected` |

When the window expires (all timestamps pruned) the pool returns to `Healthy` automatically.

##### Local child stall handling

When a local child's bdev desc timeout fires:

1. The child is marked `TimedOut` in the nexus child list.
2. A short wait (10–20 s) is observed for the path to recover transiently — much shorter than the
   normal I/O error path (default 10 min) since path-down is expected to be transient.
3. **If I/O resumes within the wait window**: child transitions back to `Online`; no rebuild.
4. **If I/O does not resume**: child is marked faulted and a full rebuild is initiated.

> **Known limitation**: removing a child from a nexus requires a subsystem pause, which cannot
> complete while I/O is pending. `aio_cancel` is being investigated as a workaround. If
> unavailable, the child stays in a timed-out state, the volume is degraded, and user intervention
> (or NVMe-oF `controller_loss_tmo`) is required to recover. This will be documented in the
> operational runbook.
>
> When multiple local children on the same pool are simultaneously timed out, a configurable limit
> on concurrent timed-out children before triggering full rebuilds is planned for a follow-up.
#### Control-Plane Scheduling Integration

The control-plane's replica-placement scheduler consults `PoolAlertStatus` before selecting a pool:

| Alert Status | Scheduler Action |
|---|---|
| `Healthy` / `Attention` | Pool is eligible for replica placement. |
| `Warning` | Pool is deprioritised; other pools are preferred when available. |
| `Critical` | Pool is excluded from replica creation, deletion, and sharing until the alert is cleared or manually overridden. |

> A `Critical` pool is still visible and its existing data is intact. The restriction is on
> **new I/O-generating control-plane operations**, not on the data path of existing nexus
> connections.

The control-plane also applies a **backoff on failed pool gRPC calls** (not just imports). When
a pool-level gRPC call fails (e.g., replica delete on a disk that is failing I/O), the call is
retried with exponential backoff to prevent log flooding and reconcile storms.

#### Import Backoff

When a pool import fails (or the post-import probe reports errors), the control-plane records a
`PoolDiag.import_ts` timestamp:

```rust
pub struct PoolDiag {
    pub import_errors: Vec<PoolDiskError>,
    pub import_ts: Option<std::time::Instant>,
}
```

The reconciler inspects `import_ts` and applies a configurable backoff (up to a maximum of 1 hour)
before reattempting the import. This prevents the current behaviour where failed imports are
retried in a tight loop.

**Special case**: if the probe returns `InvalidSuperBlock` or `ForeignPoolUid`, the pool may be
cordoned immediately (no further retries) since these errors are unlikely to be transient.

#### io-engine gRPC API Changes

Additions to the existing `Pool` message and `PoolRpc` service:

```protobuf
message Pool {
  // ... existing fields ...
  repeated Disk disk_info = 100;
  optional PoolErrors errors = 102;
}

message Disk {
  string uri = 1;
  IoStats disk_stats = 2;
  PoolErrors errors = 3;
}

service PoolRpc {
  // ... existing RPCs ...
  rpc ClearErrors(ClearErrorRequest) returns (google.protobuf.Empty) {}
}

message ClearErrorRequest {
  string name = 1;
  optional string uuid = 2;
  // If specified, only errors for these disks are cleared.
  repeated string disks = 3;
  ClearErrors clear = 4;
}

enum ClearErrors {
  // Clears all counted errors and related alerts.
  // Note: clears stall transition count but does NOT clear an active stall.
  ClearAll = 0;
}

enum PoolState {
  POOL_UNKNOWN   = 0;
  POOL_ONLINE    = 1;
  POOL_DEGRADED  = 2;
  POOL_FAULTED   = 3;
  POOL_SUSPECTED = 4;
}
```

#### Control-Plane gRPC API Changes

The control-plane's pool representation gains a `diag` field:

```protobuf
message Pool {
  optional PoolDefinition definition = 1;
  optional PoolState state = 2;
  optional PoolDiag diag = 3;
}

message PoolDiag {
  repeated DiskError import_errors = 1;
  bool stalled = 2;
}
```

`PoolState` is extended to carry `errors`:

```protobuf
message PoolState {
  // ... existing fields ...
  optional PoolErrors errors = 100;
}
```

#### RESTful OpenAPI Changes

The REST API surfaces all of the above to external consumers. New and modified schemas:

```yaml
Pool:
  properties:
    id:    { $ref: '#/components/schemas/PoolId' }
    spec:  { $ref: '#/components/schemas/PoolSpec' }
    state: { $ref: '#/components/schemas/PoolState' }
    diag:  { $ref: '#/components/schemas/PoolDiag' }
  required: [id]
  minProperties: 2

PoolDiag:
  description: Pool diagnostic information including import errors.
  properties:
    import_errors:
      type: array
      items: { $ref: '#/components/schemas/DiskError' }
  required: [import_errors]

DiskError:
  properties:
    disk:  { type: string, example: 'aio:///dev/disk/by-id/ata-xxxx' }
    error: { $ref: '#/components/schemas/PoolProbeError' }
  required: [disk, error]

PoolProbeError:
  properties:
    message: { type: string }
    code:    { $ref: '#/components/schemas/PoolProbeErrorCode' }
  required: [message, code]

PoolProbeErrorCode:
  type: string
  enum:
    - Unknown
    - DiskNotFound
    - DiskReadIoError
    - ForeignPoolName
    - ForeignPoolUid
    - SuperBlock
    - InvalidSuperBlock

PoolState:
  properties:
    # ... existing fields ...
    errorInfo: { $ref: '#/components/schemas/PoolErrorInfo' }
    diskInfo:
      type: array
      items: { $ref: '#/components/schemas/PoolDiskInfo' }

PoolErrorInfo:
  properties:
    alerts:              { $ref: '#/components/schemas/PoolAlerts' }
    ioErrorCount:        { type: integer }
    ioErrorThreshold:    { type: integer }
    ioStall:             { type: boolean }
    ioStallTransitions:  { type: integer }
  required: [alerts, ioErrorCount, ioErrorThreshold, ioStall, ioStallTransitions]

PoolDiskInfo:
  properties:
    disk:      { type: string, example: 'aio:///dev/disk/by-id/ata-xxxx' }
    errorInfo: { $ref: '#/components/schemas/PoolErrorInfo' }
  required: [disk, errorInfo]

PoolAlerts:
  properties:
    status:    { $ref: '#/components/schemas/PoolAlertStatus' }
    attention: { type: array, items: { $ref: '#/components/schemas/PoolAlert' } }
    warning:   { type: array, items: { $ref: '#/components/schemas/PoolAlert' } }
    critical:  { type: array, items: { $ref: '#/components/schemas/PoolAlert' } }
  required: [status]

PoolAlertStatus:
  type: string
  enum: [Healthy, Attention, Warning, Critical]

PoolAlert:
  type: string
  enum:
    - IoError
    - IoErrorExc
    - IoStalled
    - IoStallIntermittent
    - IoStallIntermittentExc
```

A new REST endpoint is added to acknowledge/clear pool errors:

```
DELETE /v1/pools/{id}/errors
```

#### Custom Resource Definition Changes

The `DiskPoolStatus` CRD type gains `diskInfo`, `errorInfo`, and `diag` fields, plus a
`PoolReady` condition:

```rust
pub struct DiskPoolStatus {
    pub cr_state: CrPoolState,
    pub pool_status: Option<PoolStatus>,
    pub capacity: u64,
    // ...
    pub disk_info: Option<Vec<DiskInfo>>,
    pub error_info: Option<PoolErrorInfo>,
    pub diag: Option<PoolDiag>,
    pub status: Option<PoolStatus>,
    pub error: Option<PoolError>,
    pub alert_error: Option<String>,      // combined alert + error for kubectl columns
    pub conditions: Vec<meta_v1::Condition>,
}
```

A **`PoolReady` condition** is added (following standard Kubernetes condition conventions):

| Field | Value |
|---|---|
| `type` | `PoolReady` |
| `status` | `True` (pool healthy) or `False` (pool not ready) |
| `reason` | A `PoolErrorCode` string, e.g., `DiskNotFound`, `DiskReadIoError` |
| `message` | Human-readable explanation |

A `Created` condition may also be added to distinguish "pool was never successfully created" from
"pool was online and then failed".

`PoolErrorCode` values (Rust enum, serialised as PascalCase strings in the CRD):

```
Unknown, DiskNotFound, DiskReadIoError, ForeignPoolName, ForeignPoolUid,
SuperBlock, InvalidSuperBlock, DiskIsADirectory, NodeIsUnknown, NodeIsOffline,
ImportDisabled, TimeOut, PoolDeleted, Unreachable, EncryptionSecretError
```

#### Helm Tunables

```yaml
io_engine:
  pool:
    alerts:
      # I/O error count threshold.
      # Once exceeded, pool alert is raised to Warning.
      errorThreshold: 64

      # I/O stall deadline.
      # If an I/O is stuck longer than this period the pool is considered stalled
      # and a Critical alert is raised. The pool disk will be reset; the stall is
      # cleared once I/O resumes. Defaults to io_engine.nvme.ioTimeout * 2.
      stallDeadline: ""

      # I/O stall transitions threshold.
      # After this many stall→resume transitions within the window, a Warning alert
      # is raised (IoStallIntermittentExc).
      stallTransitionThreshold: 3

      # I/O stall transitions window.
      # Time window during which stall↔resume transitions are counted.
      stallTransitionWindow: "3h"
```

### Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| **Backwards compatibility** — existing control-plane code that checks for pool presence (e.g., idempotent replica destroy) may break if a pool is now "virtually" Faulted. | Recent data-plane versions handle idempotency internally. Older version compatibility paths must be audited before the feature ships. The `diag` field is `optional` in the proto, so wire compatibility is maintained. |
| **False positives** — transient errors triggering `Warning`/cordon on a healthy pool. | The threshold is operator-tuneable. `Attention` → `Warning` escalation requires the threshold to be exceeded, not just any error. The operator can explicitly clear errors after investigation. |
| **bdev reset side effects** — reset on a shared bdev could affect unrelated pools on the same device (though single-disk pools make this unlikely). | Reset is only triggered on the specific bdev backing the stalled pool. Testing must verify no cross-pool impact. |
| **Stall detection latency** — a very short `stall_deadline` may produce spurious stalls under heavy load or slow media. | Defaulting to `2 × nvme.ioTimeout` provides reasonable headroom. Operators can tune upward. |
| **Import backoff hiding real failures** — up to 1 h delay before reattempting a pool whose disk returned transiently. | The probe can be invoked manually (via REST) to force a re-evaluation without waiting for the backoff to expire. |
| **CRD schema evolution** — adding `diskInfo`, `errorInfo`, `diag`, `conditions` to the status. | Fields are additive and all optional. A CRD schema version bump (`v1beta2` → `v1`) will be required if not already done. |
| **Local child stall: subsystem pause deadlock** — removing a timed-out child from the nexus requires a subsystem pause that cannot complete while I/O is pending, potentially leaving the nexus in a degraded state indefinitely. | Investigating `aio_cancel` as a workaround. In the interim, the NVMe-oF `controller_loss_tmo` provides a hard upper bound; behaviour is documented in the operational runbook. |
| **`POOL_HEALTH_CACHE` memory leak** — if pool export/destroy events are lost (e.g., io-engine crash), stale entries may persist indefinitely in the in-memory map. | The cache is in-memory only and is reconstructed from scratch on io-engine restart; no persistent leak across restarts. Within a running instance, export/destroy paths must be audited to ensure removal. |
| **`spdk_bdev_update_bs_desc_timeout` requires SPDK change** — adding a new SPDK C function couples the implementation to a specific spdk-rs fork. | The function is small and self-contained. It will be upstreamed to SPDK (or contributed to the spdk-rs bindings) as part of this work. |

## Drawbacks

- Adds complexity to the reconcile loop: the control-plane must now consult alert status before
  scheduling, which adds a dependency between the health subsystem and the placement subsystem.
- `PoolProbe` introduces a new RPC that must be kept in sync with import semantics; divergence
  (e.g., probe passes but import fails) produces the "half-healthy half-broken" condition that
  requires the backoff mechanism.
- Error thresholds are global per-cluster; there is no per-pool override in the initial
  implementation, which may be too coarse for heterogeneous disk fleets.

## Alternatives

### Alternative 1 — Rely solely on import error return codes (no PoolProbe)

The control-plane could simply persist the error returned by a failed import call and use that to
inform status, rather than introducing a dedicated probe RPC.

**Rejected** because:

1. This keeps the tight import-retry loop until the disk returns or the pool is deleted.
2. The import call cannot distinguish "disk gone" from "transient error" without the extra probe
   checks (read I/O, superblock validation).
3. Log flooding continues until the operator intervenes.

### Alternative 2 — Place error threshold logic in the data-plane

The data-plane could raise an internal alert and automatically unload the pool when `io_error_count`
exceeds the threshold, rather than propagating the raw count to the control-plane.

**Rejected** because the control-plane is better positioned to make holistic scheduling decisions
(taking into account volume topology, replica counts, and cluster-wide health) that a single node's
data-plane cannot see. The threshold and backoff logic therefore live in the control-plane; the
data-plane only reports metrics.

### Alternative 3 — Use SMART / NVMe health logs instead of runtime I/O error counts

Preemptively identify failing disks using firmware-reported health data.

**Deferred** to another OEP. SMART integration is complementary; this OEP focuses on runtime
observable failures which do not require firmware cooperation.

## Testing

### BDD: Disk Hot-Removal Handling

```gherkin
Feature: Disk Failure/Hot-Removal Handling

  Background:
    Given a k8s cluster with OpenEBS Mayastor installed

  Scenario: Creating a DiskPool on a missing disk reports this clearly
    Given a user creates a DiskPool CR for a missing disk
    Then the DiskPool CR cr_state should remain as Creating
    And the PoolReady condition status shall be False
    And the PoolReady condition reason shall be DiskNotFound

  Scenario: Importing a DiskPool on a missing disk reports this clearly
    Given an Online DiskPool
    When the io-engine or node are restarted
    And the disk path is missing on the node
    Then the DiskPool should eventually fail to be imported
    And the DiskPool should transition to Offline
    And the PoolReady condition status shall be False
    And the PoolReady condition reason shall be DiskNotFound

  Scenario Outline: DiskPool reflects status when disk is failed/detached from the node
    Given an Online DiskPool
    When the backing disk of the DiskPool is <event> from the node
    Then the DiskPool CR should transition to Offline
    Examples:
      | event    |
      | detached |
      | fails    |

  Scenario: Hot-removed disk is not used until re-attached
    Given two Online DiskPools on distinct nodes
    And a 2-replica volume constrained to these pools
    And a fio workload running against the volume
    When the backing disk of one DiskPool is hot-removed from the node
    Then the DiskPool CR should transition to Offline
    And the volume should eventually become Degraded
    And no partial rebuild should ever be attempted while the disk is absent
    When the disk is re-attached
    Then the DiskPool should eventually become Online again
    And if within the partial rebuild window, a partial rebuild should be attempted
    And the volume should become Online again
```

### BDD: Disk I/O Error Handling

```gherkin
Feature: Disk Failure with I/O Errors

  Background:
    Given a k8s cluster with OpenEBS Mayastor installed

  Scenario: Creating a DiskPool on a broken disk failing I/O
    Given a user creates a DiskPool CR for a broken disk with failing I/O
    Then the DiskPool CR cr_state should remain as Creating
    And the PoolReady condition status shall be False
    And the PoolReady condition reason shall be DiskReadIoError
    And the DiskPool CR pool_status should not be Unknown

  Scenario: A DiskPool disk failing I/O is reported
    Given an Online DiskPool
    When the backing disk starts failing I/O
    Then the DiskPool error count should be greater than 0
    And the DiskPool alert status should be Attention

  Scenario: I/O error count escalates alert status
    Given an Online DiskPool
    And the DiskPool alert status is Healthy
    When the backing disk starts failing I/O
    Then the DiskPool error count should be greater than 0
    And while error count is less than the configured error threshold
    Then the DiskPool alert status should be Attention
    When the error count exceeds the configured error threshold
    Then the DiskPool alert status should be Warning
    And the DiskPool state should be Suspected

  Scenario: Failing to import a DiskPool with I/O errors exposes diagnostics
    Given an Online DiskPool
    When the backing disk goes bad
    And the io-engine or node are restarted
    Then the DiskPool should eventually fail to be imported
    And the DiskPool diagnostics should show import errors
    And the PoolReady condition status shall eventually be False
    And the PoolReady condition reason shall be DiskReadIoError
```

### BDD: I/O Stall Handling

> **Timing notes**: `control_plane.cache_polling_interval` defaults to 30 s and can be overridden
> via Helm to reduce test execution time. For `Online → Suspected` transitions both
> `stall_deadline` and `cache_polling_interval` must be taken into account. For
> `Suspected → Online` transitions only `cache_polling_interval` is relevant.

```gherkin
Feature: DiskPool IO Timeout Handling

  Background:
    Given a k8s cluster with OpenEBS Mayastor installed

  Scenario: No stall when stall_deadline is disabled (set to 0)
    Given a DiskPool is created with stall_deadline set to 0
    When a DiskPool contains a local child of a published volume
    And the application starts issuing I/O to the volume
    And the DiskPool backend device I/O is stuck
    Then the application will not see any acknowledgement
    And the DiskPool status remains Online

  Scenario Outline: Pool I/O timeout transitions pool to Suspected
    Given a DiskPool is created with <stall_deadline>
    When the DiskPool backend device I/O is stuck
    And a grow-pool request is issued by annotating the DSP Custom Resource
    Then the DiskPool status is set to Suspected eventually
    And the DiskPool PoolAlertStatus is set to Critical
    When the DiskPool backend device I/O resumes
    Then the DiskPool status is Online eventually
    And the DiskPool PoolAlertStatus is set to Attention
    Examples:
      | stall_deadline |
      | 10s            |
      | 20s            |
      | 30s            |

  Scenario: Local child I/O timeout transitions pool to Suspected
    Given a DiskPool is created with stall_deadline set to 10s
    When a DiskPool contains a local child of a published volume
    And an application starts issuing I/O to the volume
    And the DiskPool backend device I/O is stuck
    Then the application will not see any acknowledgement
    And the DiskPool status is set to Suspected eventually
    And the DiskPool PoolAlertStatus is set to Critical
    When the DiskPool backend device I/O resumes
    Then the DiskPool state should be Online eventually
    And the DiskPool PoolAlertStatus is set to Attention

  Scenario: Pool re-imported with globally configured stall_deadline
    Given a DiskPool is created with no per-pool timeout
    And the io-engine DaemonSet is configured with stall_deadline set to 10s
    When the io-engine pod is restarted and the pool is re-imported
    And the DiskPool backend device I/O is stuck
    And a grow-pool request is issued by annotating the DSP Custom Resource
    Then the DiskPool status is set to Suspected eventually
    And the DiskPool PoolAlertStatus is set to Critical
    When the DiskPool backend device I/O resumes
    Then the DiskPool status is Online eventually
    And the DiskPool PoolAlertStatus is set to Attention

  Scenario: Critical pool is excluded from new volume scheduling
    Given a three-node Mayastor cluster where diskpool stall_deadline is set to 10s
    And DiskPools are created on each node
    And a three-replica volume is scheduled successfully
    And a FIO application is attached to the volume
    When the DiskPool backend device I/O is stuck
    Then the DiskPool PoolAlertStatus is set to Critical after at least 45 seconds
    And the critical reason array contains IoStalled
    And PoolState is set to Suspected
    When a new 3-replica PVC is applied
    Then the volume creation fails (cannot satisfy 3-replica placement)
    When a new 2-replica volume is created
    Then the volume is created successfully
    When the new volume's replica count is scaled up to 3
    Then the scale-up does not succeed while the pool is Critical
    When the DiskPool backend device I/O resumes
    Then the scale-up succeeds eventually
    And the 3-replica volume stuck on create transitions to Bound
    And PoolState is set to Online eventually
    And the DiskPool PoolAlertStatus is set to Attention

  Scenario Outline: PoolAlertStatus resets to Healthy when transition window expires
    Given a Pool created with <stall_transition_window>, <stall_transition_threshold>, and <stall_deadline>
    And a replica is scheduled on the Pool with its target on the same node
    And FIO is running on that volume
    When the DiskPool backend device I/O stalls at time t
    Then the DiskPool PoolAlertStatus is set to Critical eventually
    And PoolState is Suspected
    When the DiskPool backend device I/O resumes
    Then the DiskPool PoolAlertStatus is set to Attention eventually
    When t + <stall_transition_window> elapses
    Then the DiskPool PoolAlertStatus is set to Healthy
    Examples:
      | stall_transition_window | stall_transition_threshold | stall_deadline |
      | 300s                    | 5                          | 10s            |

  Scenario Outline: stall_transition_threshold exceeded promotes pool to Warning
    # Timeline (all times relative to t=0):
    #   t=0:   first stall
    #   t=0+:  I/O resumes (transition 1)
    #   ... stall/resume 3 more times before t+300 (transitions 2-4)
    #   t=320: stall again; I/O resumes immediately (transition 5 — first one is now outside window)
    #          → still only 4 valid transitions: Attention
    #   t=350: stall again → Critical while stalled
    #          on resume → Warning (IoStallIntermittent, 5 transitions in window)
    #   t=400: oldest transition record expires → Attention
    #   t=650: all records expire → Healthy
    Given a Pool created with <stall_transition_window>, <stall_transition_threshold>, and <stall_deadline>
    And a replica is scheduled on the Pool with its target on the same node
    And FIO is running on that volume
    When the DiskPool backend device I/O stalls at time t
    Then the DiskPool PoolAlertStatus is set to Critical after <stall_deadline>
    And PoolState is set to Suspected eventually
    When the DiskPool backend device I/O resumes
    Then the DiskPool PoolAlertStatus is set to Attention eventually
    And the DiskPool backend device I/O stalls and resumes 3 more times before t+300
    When the DiskPool backend device I/O stalls at t+320 and immediately resumes
    Then the DiskPool PoolAlertStatus is set to Attention eventually
    # 5th stall within the window triggers Warning on recovery
    When the DiskPool backend device I/O stalls at t+350
    Then the DiskPool PoolAlertStatus is set to Critical eventually
    And the critical reason array contains IoStallIntermittent
    And PoolState is set to Suspected eventually
    When the DiskPool backend device I/O resumes
    Then the DiskPool PoolAlertStatus is set to Warning eventually
    When t+400 elapses
    Then the DiskPool PoolAlertStatus is set to Attention eventually
    When t+650 elapses
    Then the DiskPool PoolAlertStatus is set to Healthy eventually
    Examples:
      | stall_transition_window | stall_transition_threshold | stall_deadline |
      | 300s                    | 5                          | 10s            |
```

### BDD: Error Clearing

```gherkin
Feature: Pool Error Clearing

  Background:
    Given a k8s cluster with OpenEBS Mayastor installed

  Scenario: Operator clears errors after disk replacement
    Given a DiskPool in Warning state due to I/O errors
    When the operator replaces the disk and calls ClearErrors
    Then the DiskPool error count should be 0
    And the DiskPool alert status should return to Healthy
    And the DiskPool state should return to Online
```
