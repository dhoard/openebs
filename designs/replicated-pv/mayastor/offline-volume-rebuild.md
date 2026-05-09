---
oep-number: OEP 1977
title: Offline Volume Rebuild
authors:
  - "@yugchaudhari"
owners:
  - "@yugchaudhari"
editor: TBD
creation-date: 2026-05-09
last-updated: 2026-05-09
status: provisional
see-also:
  - https://github.com/openebs/mayastor/issues/1977
---

# Offline Volume Rebuild

## Table of Contents

- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
- [Proposal](#proposal)
  - [User Stories](#user-stories)
  - [Implementation Details](#implementation-details)
    - [State Tracking](#state-tracking)
    - [Trigger Logic](#trigger-logic)
    - [Rebuild Flow](#rebuild-flow)
    - [Concurrent Operations](#concurrent-operations)
    - [Garbage Collection Interaction](#garbage-collection-interaction)
  - [Risks and Mitigations](#risks-and-mitigations)
- [Graduation Criteria](#graduation-criteria)
- [Implementation History](#implementation-history)
- [Drawbacks](#drawbacks)
- [Alternatives](#alternatives)
- [Testing](#testing)

## Summary

Mayastor currently rebuilds replicas only when a volume is **published** (i.e. has an active nexus). When a volume is unpublished and one or more of its replicas become unhealthy, no automatic rebuild occurs because the hot-spare logic depends on a nexus existing.

This OEP proposes an **offline volume rebuild** mechanism: when an unpublished volume is detected as `Degraded` for longer than a configurable threshold, the control plane temporarily creates a non-shared nexus, lets the existing rebuild engine restore the replicas, then tears the nexus down.

## Motivation

In production environments, many volumes spend most of their lifecycle in an unpublished state, typical examples:

- CDI golden images (PVCs that are read once during machine image cloning, never mounted again)
- Cold backup volumes
- Volumes attached to stopped/scaled-down workloads
- Volumes detached during node maintenance

When a DiskPool is decommissioned, a node fails, or a disk is taken offline for maintenance, replicas of these unpublished volumes go into a degraded state. Because the current hot-spare logic gates on `volume_state.target.is_some()`, no automatic rebuild is triggered. Operators must either:

1. Manually publish each affected volume to trigger a rebuild
2. Accept silent data risk until a future client mounts the volume
3. Build external tooling to detect and remediate

This was hit recently in production while decommissioning DiskPools, only mounted/published volumes had their rebuilds triggered automatically; replicas of unpublished volumes (CDI golden images, etc.) were left degraded with no recovery path until manual intervention.

### Goals

- Detect degraded unpublished volumes via existing `NexusInfo` health signal
- Automatically trigger a rebuild for such volumes after a configurable wait period
- Reuse the existing rebuild engine and nexus lifecycle code as much as possible
- Avoid impacting workload performance, bound concurrency separately from "regular" rebuilds
- Handle concurrent publish/unpublish gracefully

### Non-Goals

- Replica-to-replica rebuild without a nexus (the existing rebuild engine runs inside the nexus)
- Cross-node replica relocation (the temp nexus runs on the same node as a healthy replica)
- New data-plane API changes, the io-engine already supports unshared nexus creation; only control plane changes are needed
- Replacing or significantly changing the existing hot-spare logic for published volumes

## Proposal

### User Stories

#### Story 1: DiskPool decommissioning

A cluster operator wants to decommission a DiskPool to retire ageing hardware. They run `kubectl mayastor cordon pool ...` and wait for replicas to be evicted. With offline rebuild enabled, replicas of unpublished volumes are automatically rebuilt onto other pools without operator intervention. Without offline rebuild, the operator must either publish every affected volume manually or accept that those replicas will remain on the cordoned pool.

#### Story 2: Node maintenance

A node is taken offline for maintenance. After it returns, replicas of unpublished volumes that resided on the node are stale (`NexusInfo` reflects this). With offline rebuild, after the configured grace period these replicas are rebuilt automatically. The grace period prevents needless rebuild work for nodes that come back quickly.

#### Story 3: Disk failure on a cold volume

A physical disk hosting a replica of an unpublished volume fails. Without offline rebuild, the volume silently runs at reduced redundancy until the next publish. With offline rebuild, recovery is automatic.

### Implementation Details

#### State Tracking

The temporary nexus used for offline rebuild needs to be tracked across control-plane restarts. Proposed approach: **reuse the existing `target_config` mechanism** with `share = Protocol::None`. The existing `published()` helper (`OperationGuardArc::<VolumeSpec>::published()`) returns `target().is_some()`, so an unshared target still counts as "published" from most code paths' perspective, which is desirable, because:

- GC code that gates on "target exists" continues to protect replicas during rebuild
- `health_info_id()` continues to point at the active nexus's `NexusInfo`
- The existing nexus reconciler (`handle_faulted_children`) will work without modification

A small flag (e.g. `target_config.target.share_state == OfflineRebuild`) can distinguish offline-rebuild targets from real publishes for the few places that need the distinction (CSI publish flow, telemetry).

#### Trigger Logic

A new `OfflineRebuildReconciler` is added to `controller/reconciler/volume/mod.rs` alongside `HotSpareReconciler` and `GarbageCollector`. Trigger conditions per volume:

- `volume.policy.self_heal == true`
- `volume.target().is_none()`: volume is currently unpublished
- `volume_state.status == Degraded`, `online_clean_replicas < num_replicas` per `NexusInfo`
- `health_info_id().is_some()`, there is data to rebuild from
- Time since the volume entered the `Degraded` state exceeds the offline rebuild grace period

If `current_replica_count < num_replicas` (a replica is missing entirely, not merely degraded), the urgent path applies, no grace period.

#### Rebuild Flow

1. Pick a target node via existing `target_node_candidate`. Prefer a node hosting a healthy replica to avoid cross-node copy.
2. Build a `TargetConfig` with `share = Protocol::None` (unshared).
3. Reuse `volume/operations_helper.rs::create_nexus`. It already calls `healthy_volume_replicas`, makes replicas accessible, and creates the nexus via `OperationGuardArc::<NexusSpec>::create`.
4. Skip the `share_nexus` step entirely. A `CreateNexus` without subsequent share is naturally non-shared.
5. The existing `nexus_reconciler` will detect faulted children and trigger rebuild via `handle_faulted_children` (with a possibly shorter `faulted_child_wait` for offline rebuilds, since there is no I/O cost in starting immediately).
6. On rebuild completion (`online_clean_replicas == num_replicas`), tear down the nexus via `nexus.destroy(...).with_disown(volume_uuid)` and clear `target_config`.

The temporary nexus is created with read-only / "rebuild-only" semantics where possible (per Tiago's point 1 on the issue) so that a control-plane crash during rebuild does not force a full re-rebuild on recovery, there is no client-side dirty data to worry about.

#### Concurrent Operations

**User publishes during offline rebuild.** Two cases:

1. **CSI publish does not request a specific target node**: the simplest path is to "promote" the existing offline-rebuild nexus to a published nexus by calling `share_nexus`. No state migration needed.
2. **CSI publish explicitly requests a node different from the rebuild nexus's node**: the offline-rebuild nexus is destroyed and a fresh publish flow runs on the requested node. The replicas remain (GC keeps them) and the published nexus's reconciler will resume the rebuild.

**User unpublishes during offline rebuild.** This case does not apply directly: the rebuild was already initiated for an unpublished volume. If the volume was published *during* rebuild and is now being unpublished, follow Tiago's point 6 on the issue: drop the share but keep the nexus running until rebuild completes, then tear it down.

#### Garbage Collection Interaction

`garbage_collector.rs::disown_unused_replicas` skips disowning when `target_nexus_rsc.is_none()`, for offline rebuild we *will* have a target (an unshared one), so this check passes naturally. The replicas being rebuilt are referenced by the temporary nexus's children, so they will not be considered "unused". This is safe by construction.

One caveat: a replica that was in the process of being disowned right before offline rebuild kicks in might race with the temp nexus creation. The operation guard on the volume prevents concurrent volume-level operations, so this should be naturally serialized. To be verified during implementation.

### Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| Storms of offline rebuilds after a node failure overwhelm the cluster | Separate concurrency limit for offline rebuilds (e.g. `max_offline_rebuilds` config), defaulting to a low value (1–2) |
| Offline rebuild competes with active workload I/O | Schedule rebuild on idle nodes preferentially; consider rebuild rate limits |
| Transient node flakiness triggers unnecessary rebuilds | Configurable grace period (e.g. 10 minutes default), separate from the published-volume `faulted_child_wait` |
| Crash during rebuild leaves orphaned temp nexus | The nexus is owned by the volume (`owner = Some(volume_uuid)`); existing reconciler logic should detect and clean up. To be confirmed during implementation. |
| Published volume target node conflicts with rebuild node | Documented in [Concurrent Operations](#concurrent-operations) |

## Graduation Criteria

**Provisional → Implementable**

- Design questions in this OEP resolved with maintainer review
- POC implementation in a fork demonstrates the approach works for at least User Stories 1 and 2
- Open question on `health_info_id` reuse confirmed empirically

**Implementable → Implemented**

- Full reconciler implementation merged
- BDD tests covering all three user stories pass
- Documentation added to user-facing docs explaining when offline rebuild kicks in and how to configure grace period / concurrency
- Configuration knobs (`offline_rebuild_grace_period`, `max_offline_rebuilds`, enable/disable flag) wired through helm chart values

## Implementation History

- 2026-05-01: Issue [openebs/mayastor#1977](https://github.com/openebs/mayastor/issues/1977) opened by @tiagolobocastro
- 2026-05-08: @yugchaudhari volunteered to implement; design discussion in issue comments
- 2026-05-09: Provisional OEP submitted

## Drawbacks

- Adds another reconciler to the volume reconcile loop, slightly increasing per-cycle cost
- New configuration surface (grace period, concurrency limit) to maintain and document
- Subtle interaction with publish/unpublish flows, careful test coverage required

## Alternatives

### Replica-to-replica rebuild without a nexus

Theoretically possible but requires significant data-plane changes (the rebuild engine currently lives inside the nexus). Out of scope for this OEP.

### Manual rebuild API

Expose an admin-only API to trigger rebuild for a specific unpublished volume. Doesn't address the operator-burden problem at scale; would still need detection logic. Considered as a fallback if the automatic approach is rejected, but not preferred.

### Auto-publish / auto-unpublish

Briefly publish the volume to a control-plane-owned target, let the existing hot-spare logic do its thing, then unpublish. Functionally similar to the proposed approach, but more disruptive, every offline rebuild would briefly expose the volume on the network. The proposed unshared-nexus approach is strictly better.

## Testing

### Unit / component tests

- New `OfflineRebuildReconciler` unit tests covering trigger conditions
- `target_config` accessor changes (if any) covered by existing volume spec tests

### BDD tests

New scenarios in `tests/bdd/features/snapshot/` (or a new `tests/bdd/features/rebuild/offline/` directory):

```gherkin
Feature: Offline volume rebuild

  Scenario: Replica of unpublished volume is rebuilt after grace period
    Given an unpublished volume with 2 replicas across nodes A and B
    When node A is taken offline for longer than the offline rebuild grace period
    Then a temporary nexus should be created on a remaining healthy node
    And the volume should return to Online state once rebuild completes
    And the temporary nexus should be torn down

  Scenario: Offline rebuild does not trigger before grace period
    Given an unpublished volume with 2 replicas across nodes A and B
    When node A is taken offline briefly (less than grace period) and returns
    Then no offline rebuild should have been triggered

  Scenario: Publish during offline rebuild promotes the nexus
    Given an offline rebuild is in progress on an unpublished volume
    When the user publishes the volume without specifying a target node
    Then the temporary nexus should be promoted to a published nexus via share
    And the rebuild should continue uninterrupted

  Scenario: Replica missing entirely triggers immediate rebuild
    Given an unpublished volume whose replica count fell below the required count
    When the offline rebuild reconciler runs
    Then a rebuild should be triggered without waiting for the grace period

  Scenario: Concurrency limit is respected
    Given the offline rebuild concurrency limit is 1
    And two unpublished volumes are eligible for offline rebuild
    Then at most one offline rebuild should be in progress at a time
```

### Production-like tests

Extend the existing pool cordon / decommission tests to include unpublished volumes and assert their replicas are rebuilt onto other pools without operator intervention.
