---
oep-number: OEP 4242
title: ReadOnlyMany (ROX) Support for Filesystem-Mode Volumes
authors:
  - "@yugchaudhari"
owners:
  - "@yugchaudhari"
editor: TBD
creation-date: 2026-06-09
last-updated: 2026-06-22
status: implementable
see-also:
  - https://github.com/openebs/openebs/issues/4242
  - https://github.com/openebs/mayastor/issues/1993
  - https://github.com/openebs/openebs/issues/4059
---

# ReadOnlyMany (ROX) Support for Filesystem-Mode Volumes

## Table of Contents

- [Summary](#summary)
- [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
  - [Out of Scope](#out-of-scope)
- [Proposal](#proposal)
  - [User Stories](#user-stories)
  - [Implementation Details](#implementation-details)
    - [CSI Controller](#csi-controller)
    - [CSI Node](#csi-node)
    - [Data Plane (Nexus)](#data-plane-nexus)
    - [Configuration and Opt-In](#configuration-and-opt-in)
    - [Lifecycle (RWO to ROX, K8s-side)](#lifecycle-rwo-to-rox-k8s-side)
  - [Risks and Mitigations](#risks-and-mitigations)
- [Graduation Criteria](#graduation-criteria)
- [Implementation History](#implementation-history)
- [Drawbacks](#drawbacks)
- [Alternatives](#alternatives)
- [Testing](#testing)

## Summary

Mayastor today rejects the `ReadOnlyMany` (ROX) access mode for filesystem-mode volumes at the CSI controller. Workloads that need a "fill once, read from many pods" lifecycle (model serving with KServe/vLLM, ComfyUI workflows, immutable datasets, golden images) have no first-class way to share a single volume across N reader pods without either copying the data per pod or layering NFS on top.

This OEP proposes accepting `MultiNodeReaderOnly` for filesystem-mode volumes, reusing the multi-host NVMe-oF attach path that already ships in v2.11.0 for RWX block, and enforcing read-only end-to-end with a `readOnly` flag on the nexus so a misconfigured mount cannot write through.

## Motivation

KServe scales out vLLM inference pods across GPU nodes. Each pod loads the same model from a POSIX filesystem PV at startup: `*.safetensors`, `tokenizer.json`, `config.json`, etc. After load the model is immutable; the PV is read-only from the application's perspective. GPUs use GPU Direct Storage (GDS) to DMA model weights directly from the NIC into GPU memory, which requires a direct NVMe-oF target the kernel can DMA from. NFS or any protocol layer between the NIC and the GPU breaks GDS.

What's missing is "one PV, mounted concurrently from N inference pods, read-only". Models are 10s to 100s of GiB. Provisioning a copy per pod wastes storage and stalls autoscale on cold start. The standard Kubernetes primitive for that pattern is `ReadOnlyMany`, and mayastor's CSI controller currently rejects it for `AccessType::Mount`.

The multi-host data path is no longer the gap: v2.11.0 shipped RWX block (experimental) for KubeVirt and uses the multi-initiator NVMe-oF attach we'd need here. ROX-fs is the narrower incremental ask on top: accept ROX in the CSI layer, mount read-only, enforce read-only at the data plane.

### Goals

- Accept `MultiNodeReaderOnly` for filesystem-mode (Mount) volumes in the CSI controller.
- Allow `ControllerPublishVolume` to publish a ROX volume with `readonly: true`.
- Mount with `ro,noload` (ext4) and `ro,norecovery` (xfs) in `NodeStageVolume` and `NodePublishVolume`, end-to-end read-only.
- Enforce read-only at the nexus layer via a `readOnly` flag so a misconfigured mount above cannot write through.
- Reuse the multi-host attach path shipped for RWX block (`allowed_hosts` propagation for ROX initiators, multi-initiator NVMe-oF target).
- Opt-in via a `StorageClass` parameter, mirroring the existing `rwx_block` pattern.

### Non-Goals

- ROX for raw block volumes. This OEP is scoped to filesystem-mode volumes.
- Mayastor-orchestrated RWO to ROX transition. K8s composes the lifecycle (`Retain` + sequential PVCs); mayastor only accepts the access mode requested on each publish.
- Multi-writer semantics of any kind. ROX is reader-only by construction; there is no concurrent-write story to address here.
- Cross-version filesystem consistency or read-after-write coherence between RWO writer and ROX readers. The K8s-side lifecycle hands off cleanly: the writer pod exits before the PV transitions to ROX.

### Out of Scope

These came up during the issue review and are explicitly out of scope for this OEP:

- **`VolumeAttributesClass` / `ControllerModifyVolume`**: not required because the K8s-side `Retain` + sequential PVC reuse composes the lifecycle without changing volume attributes in place. `ControllerModifyVolume` is independently unimplemented in mayastor; should land that is its own OEP.
- **`VolumeMaintenance` mode**: a useful primitive for fsck/grow/migrate flows beyond ROX, but its own work item. Tracked separately.
- **`fsck` during ROX**: the volume is written exactly once with a clean unmount and mounted `ro,noload` / `ro,norecovery` thereafter; no journal replay, no metadata writes. If corruption is ever detected, recovery is operational: scale readers to zero, re-attach RWO on a single node, fsck, re-attach ROX.
- **Filesystem-consistent snapshots during ROX**: the source is byte-stable while in ROX, so `FIFREEZE` is a no-op for consistency. Snapshot freeze is only required at the RWO to ROX boundary, where it is a single-writer freeze on the loader mount, same as today.

## Proposal

### User Stories

**Story 1: KServe / vLLM model serving**

As a platform engineer running KServe-managed vLLM inference pods, I want to load model weights from a single PV across N pods on different GPU nodes, using NVMe-oF over RDMA for GDS-backed DMA into GPU memory, without copying the model per pod or interposing NFS.

**Story 2: ComfyUI / image-generation workflows**

As an ML user running ComfyUI fan-out workflows, I want N worker pods to read the same model checkpoints, LoRAs, and shared assets from a single PV, with predictable cold-start latency on autoscale.

**Story 3: Golden images and immutable datasets**

As an operator, I want immutable PVs (golden images, reference datasets, cached package mirrors) to be readable concurrently from many pods without provisioning per-pod copies.

### Implementation Details

The implementation breaks down by CSI/data layer.

#### CSI Controller

Located in `control-plane/csi-driver/src/bin/controller/controller.rs`.

1. **Accept `MultiNodeReaderOnly` in `check_volume_capability` for `AccessType::Mount`**.

   Currently the function accepts only `SingleNodeWriter` for Mount and falls into a catchall `_ => Err("Invalid volume access mode")` for everything else (including `MultiNodeReaderOnly`). Extend the match to accept `MultiNodeReaderOnly` when the volume's StorageClass opts in.

2. **Drop the unconditional reject on `args.readonly` in `ControllerPublishVolume`**.

   Today there is an unconditional `if args.readonly { return Err("Read-only volumes are not supported") }`. Replace with a check that allows `readonly: true` when the published access mode is `MultiNodeReaderOnly`.

3. **Advertise the capability**.

   `ControllerGetCapabilities` / `NodeGetCapabilities` may need a corresponding update so the side-car selects the correct access-mode handling.

#### CSI Node

Located in the CSI node binary.

1. **Allow `readonly` in `NodeStageVolume` and `NodePublishVolume`** for volumes whose access mode is `MultiNodeReaderOnly`. The node-side `check_access_mode` already has a branch for `MultiNodeReaderOnly`; the gating today is purely the controller-side rejection that prevents the request from reaching the node.

2. **Mount with read-only filesystem options**.

   - ext4: `ro,noload` (skip journal replay; the volume was unmounted cleanly in the writer phase, no recovery needed).
   - xfs: `ro,norecovery` (same intent for xfs log replay).
   - General: `MS_RDONLY` end-to-end so even a buggy client cannot mutate the filesystem from userspace.

3. **Bind-mount the staged path** into the pod with `readOnly` as today; nothing new beyond filesystem options at stage time.

#### Data Plane (Nexus)

A `readOnly` flag on the nexus enforces read-only at the data plane, so a misconfigured mount above cannot write through.

- **Nexus-level enforcement**: when a nexus is created with `readOnly: true`, write I/O is refused at the nexus and never reaches the underlying replicas. Defense in depth: even if the upstream mount somehow gets `rw`, replica data cannot be mutated.
- **Initiator semantics**: under multi-host attach, all initiators share the same read-only nexus. There is no per-initiator read/write differentiation, which matches the access-mode semantics (`MultiNodeReaderOnly` is "all initiators read-only").
- **Plumbing**: the `readOnly` flag is set on `target_config` at publish time when the access mode is `MultiNodeReaderOnly`. It is persisted with the publish operation and cleared on unpublish, same lifecycle as other publish-scoped attributes.

#### Configuration and Opt-In

Opt-in via a `StorageClass` parameter, mirroring `rwx_block`.

- New SC parameter, suggested name `rox_fs: "true"` (or equivalent). Default off; behavior is strictly additive.
- The CSI controller checks the SC parameter at provisioning time and tags the volume as ROX-capable. Without the opt-in, the existing rejection path is preserved.

This keeps existing users on the existing behavior and gives ROX users a single, explicit knob.

#### Lifecycle (RWO to ROX, K8s-side)

The RWO to ROX transition is composed on the K8s side; mayastor is not responsible for orchestrating it.

1. Create writer PVC (RWO) on a StorageClass with `reclaimPolicy: Retain`.
2. Writer pod mounts, writes the model artifacts, exits cleanly (filesystem unmounted).
3. Delete the writer PVC. The PV transitions to `Released`; the underlying mayastor volume and replicas are untouched because of `Retain`.
4. Operator (or controller) clears `spec.claimRef` and updates `spec.accessModes` on the PV.
5. Create reader PVC (ROX) bound to the same PV (`volumeName: <pv-name>`).
6. N inference pods mount the reader PVC with `readOnly: true`.

This pattern was empirically verified end-to-end on a 1-node kind cluster (hostPath PV), including two concurrent reader pods both mounting the ROX PVC with `readOnly: true`.

### Risks and Mitigations

#### Network Partition

The multi-host NVMe-oF attach path that ROX-fs reuses ships as experimental in v2.11.0 specifically because partition handling is incomplete. ROX-fs is less exposed than RWX block because there is no writer, so there is no risk of two sides diverging on a write. The failure mode reduces to "some readers go stale on one side of the partition" rather than "two writers conflict".

**Mitigation**: document the partition behavior explicitly in the user-facing notes. The graduation criteria below requires the multi-host attach path to graduate from experimental before ROX-fs itself can graduate to GA.

#### Misconfigured Mount

A user or operator could in principle craft a mount with `rw` flags despite the access-mode declaration, or a bug in the node side could fail to set `MS_RDONLY`.

**Mitigation**: the nexus-level `readOnly` flag refuses writes at the data plane. Defense in depth: even if userspace requests `rw`, no replica is mutated.

#### Concurrent Writer During ROX

A misbehaving operator could attempt to mount RWO while ROX readers are attached.

**Mitigation**: the nexus rejects the RWO publish on a volume currently published as ROX. Same volume cannot be published with two different access modes concurrently. Explicit error to the caller.

## Graduation Criteria

- All goals implemented behind the SC opt-in.
- Integration tests covering the full RWO to ROX lifecycle: provision RWO, write, unpublish, re-publish ROX, multi-host mount, verify read-only, scale readers up and down.
- The multi-host NVMe-oF attach path graduates from experimental (partition handling completed) before ROX-fs graduates to GA. Until then, ROX-fs ships as experimental as well.
- Documentation: user guide for the RWO to ROX K8s lifecycle, SC parameter reference, mount option defaults.

## Implementation History

- 2026-06-09: OEP drafted, status `provisional`.
- TBD: implementation PR(s) opened.

## Drawbacks

- Adds a new SC parameter to the API surface. Mitigated by mirroring the existing `rwx_block` pattern so the convention is consistent.
- Reuses the experimental multi-host attach path; inherits its partition-handling limitations until that work graduates.
- Operational responsibility for the RWO to ROX transition sits with the K8s-side operator (or a controller they author). Mayastor does not orchestrate it. This is intentional but worth calling out as a knob the user owns.

## Alternatives

**NFS layered on an RWO volume**: the documented openebs workaround for multi-reader access. It breaks GDS: `nvidia-fs`/`cuFile` rely on a direct NVMe target the kernel can DMA from. NFS interposes protocol layers between the NIC and the GPU and loses the direct NIC-to-GPU DMA path GDS depends on. Non-starter for GDS workloads.

**One RWO PV per pod (clones)**: wastes storage (model size × N pods, where models are 10s to 100s of GiB), and the per-clone provision step adds latency to autoscale-up exactly on cold start. Clones are the right answer when readers actually diverge or need writes, not when bytes are byte-identical across everyone.

**Application-layer fanout / shared in-memory caches**: doesn't help the cold-start case where every fresh pod needs to load weights from durable storage.

**`MULTI_NODE_SINGLE_WRITER`**: CSI spec allows it but K8s does not expose a corresponding `accessModes` value, so it cannot drive a K8s-native lifecycle. Not viable as the user-facing model.

## Testing

Behaviour specification (BDD-style):

1. **Happy path**: a volume provisioned via a ROX-opted-in StorageClass, written via an RWO writer pod, re-published as ROX, attached to N reader pods on N nodes. Each reader sees the same filesystem contents. Writes from any pod fail at the userspace layer; data plane is byte-stable.
2. **Opt-out is preserved**: a volume provisioned via a non-opted-in StorageClass and a ROX-requesting PVC is rejected at `CreateVolume` / `ControllerPublishVolume`, same as today.
3. **Mount options**: ROX mounts use `ro,noload` (ext4) / `ro,norecovery` (xfs). Writes via `dd` from a pod with `readOnly: false` mount are rejected by the kernel.
4. **Nexus-level enforcement**: a forged write at the data plane (bypassing the mount layer) is refused by the nexus when `readOnly: true` is set.
5. **Concurrent attach**: N reader pods mount the same ROX PVC and each reads the model files; teardown of any reader does not affect the others.
6. **Republish across modes**: a volume previously published as RWO is unpublished, then published as ROX; new mounts honour ROX semantics.
7. **RWO during ROX is rejected**: a volume published as ROX cannot be concurrently published as RWO; explicit error.
