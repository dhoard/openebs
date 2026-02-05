---
oep-number: OEP 4146
title: VolumeAttributesClass support for local-pv lvm
authors:
  - "@rybas-dv"
owners:
  - "@tiagolobocastro"
editor: TBD
creation-date: 2026-01-11
last-updated: 2026-01-21
status: implementable
---

# VolumeAttributeClass Support for Enhanced PVC Configuration

## Table of Contents

* [Table of Contents](#table-of-contents)
* [Summary](#summary)
* [Motivation](#motivation)
    * [Goals](#goals)
    * [Non-Goals](#non-goals)
* [Proposal](#proposal)
    * [User Stories](#user-stories)
      * [Story 1: Dynamic Performance Tuning](#story-1-dynamic-performance-tuning)
      * [Story 2: Multi-Tenant Isolation Policies](#story-2-multi-tenant-isolation-policies)
    * [Implementation Details/Notes/Constraints](#implementation-detailsnotesconstraints)
    * [Risks and Mitigations](#risks-and-mitigations)
* [Graduation Criteria](#graduation-criteria)
* [Implementation History](#implementation-history)
* [Drawbacks](#drawbacks)
* [Alternatives](#alternatives)
* [Infrastructure Needed](#infrastructure-needed)
* [Testing](#testing)

## Summary

VolumeAttributeClass (VAC) is a Kubernetes resource that allows users to manage storage settings. This enhancement proposal introduces VAC support to OpenEBS local-pv lvm, allowing for more flexible and dynamic configuration of PersistentVolumeClaims (PVCs) with attributes such as iops and throughput through cgroup v2 io.max.

The implementation extends the existing CSI driver controller side to interpret and apply VolumeAttributeClass parameters during volume provisioning and management, providing a standardized way to customize storage behavior without modifying StorageClass definitions.

## Motivation

Currently, OpenEBS users face limitations when they need to apply specific volume attributes that vary between applications or environments. The primary methods available are:

1. **Multiple StorageClasses**: Creating separate StorageClass for each combination of attributes leads to proliferation and management overhead.
2. **Specific parameters**: There is no way to dynamically adjust storage settings after creation, or it is not obvious.

VolumeAttributeClass addresses these limitations by providing:
- A standardized Kubernetes-native way to specify volume attributes.
- Dynamic attribute application at PVC throughout the all life cycle.
- Validation through OpenAPI schemas.
- Better separation of concerns between storage provisioning (StorageClass/PVC) and volume characteristics (VolumeAttributeClass).

### Goals

1. Implement VolumeAttributeClass support in OpenEBS LVM local-pv.
2. Extend CSI driver to process VAC parameters during all life cycle.
3. Support backward compatibility with existing StorageClass parameters.
4. Provide validation for VolumeAttributeClass parameters.
5. Document common use cases and examples

### Non-Goals

1. Replacement of ioLimits functionality.
2. Support for VolumeAttributeClass in all OpenEBS storage engines simultaneously

## Proposal

### User Stories

#### Story 1: Dynamic Performance Tuning
As a database administrator, I want to specify different IOPS profiles for my PostgreSQL PVCs based on their workload importance, so that production databases get high-performance storage while development databases use cost-optimized storage, all using the same StorageClass.

#### Story 2: Multi-Tenant Isolation Policies
As a cluster administrator, I want to apply different QoS policies to PVCs, so that premium tenants get guaranteed performance while standard tenants use best-effort storage, using vac name in PVC to select appropriate VolumeAttributeClasses.

### Implementation Details/Notes/Constraints
Following a top-down approach, here are the required modifications:

#### 1. LVMVolume CRD Modifications for VAC Processing
When processing vac parameters, it is proposed to use unified parameters and directional ones, for example unified:
```
---
apiVersion: storage.k8s.io/v1
kind: VolumeAttributesClass
metadata:
  name: lvm-qos
driverName: local.csi.openebs.io
parameters:
  qosIopsLimit: "200"
  qosBandwithPerSec: "7000Mi"
```
or directional:
```
---
apiVersion: storage.k8s.io/v1
kind: VolumeAttributesClass
metadata:
  name: lvm-qos
driverName: local.csi.openebs.io
parameters:
  qosIopsReadLimit: "100"
  qosIopsWriteLimit: "200"
  qosBandwithReadPerSec: "7000Mi"
  qosBandwithWritePerSec: "6000Mi"
```

To interpret VAC parameters listed above such as `qos-iops-limit` or `qos-bandwidth-write-per-sec`, we need to extend the `LVMVolume` Custom Resource Definition. The logical approach is to add an `qos` field to the `LVMVolume.spec` to encapsulate performance-related parameters. Parameters in VAC can be used together, but must be checked for conflicts so that the resulting `LVMVolume.spec` resource will look like this:
```
apiVersion: local.openebs.io/v1alpha1
kind: LVMVolume
metadata:
  namespace: lvm-localpv
spec:
  capacity: "1000341504"
  qos:
    readBPS: 7340032000    # 7GB/s read throughput limit
    readIOPS: 10000        # 10K read IOPS limit
    writeBPS: 3670016000   # 3.5GB/s write throughput limit
    writeIOPS: 5000        # 5K write IOPS limit
  ownerNodeID: kworker1
  shared: "no"
  thinProvision: "no"
  vgPattern: ^lvmg$
  volGroup: lvmg
```

Implementation details:
- Add IO struct to LVMVolumeSpec in Go types.
- Include validation for parameter boundaries.
- Ensure backward compatibility (io field optional).
- Add conversion logic between VAC parameters and LVMVolume io spec.

#### 2. CSI Controller Modifications for VAC Processing
The CSI driver controller requires the RPC_MODIFY_VOLUME capability to process VolumeAttributeClass parameters. Once declared, we can obtain VAC parameters through GetMutableParameters and convert them to the LVMVolume resource format.

Implementation details:
- Add IOParams holds collection of supported settings that can be configured in VolumeAttributesClass.
- Add parses and validates VAC mutable parameters into IOParams.
- Add IOParamsCreateVolume builds the initial VolumeIO spec for volume creation.
- Add IOParamsModifyVolume applies mutable parameters to an existing VolumeIO spec.
- Implement the logic of ControllerModifyVolume

#### 3. CSI Node Agent Modifications for VAC Processing
The CSI driver node agent must apply VAC parameters to workloads in two scenarios. For both scenarios, it is partially inherent:
- Implement updateVol and syncVol handlers in node controller.
- Watch for LVMVolume updates and reapply IO limits.
- Handle volume remounts and pod restarts gracefully.
- Maintain idempotent operations for limit application.

The scenarios themselves:
- Initial Application during NodePublishVolume.
- Runtime Updates via Controller Reconciliation.

Implementation details:
- Add logic to enqueue processed VAC parameters in mgmt volume node agent.
- Add logic which tries to converge to a desired state for the VAC parameters on the node.
- Add logic which applies the VAC parameters on the node to the mounts.

#### 4. Cgroup-based IO Limiting Modifications for VAC Processing
The IO limits must be applied to both volumeMode: Block and volumeMode: Filesystem PVCs. This is achieved through cgroup v2 IO controller integration at the node level. To apply the io.max limit in /sys/fs/cgroup, you need to define the path to a specific pod. I suggest defining this path using mount points, for example: using the volume name, you can determine the lv path, which can then be used to determine where the lv is mounted, and using this information, you can determine where to apply io.

Implementation details:
- Add logic which use existing cgroup for the pod/workload.
- Add logic which write limit parameters to cgroup io.max files.
- Add logic which Modify cgroup limits when VAC parameters change

#### 5. Deletion and Lifecycle Management
PVC/PV deletion must proceed normally regardless of VAC configuration.

### Risks and Mitigations

#### Risk 1: Incorrect IO Limit Application
If VAC parameters are incorrectly processed or applied, workloads may receive unexpected IO limits (too restrictive, too permissive, or none at all).

Mitigation: Implement comprehensive validation at multiple levels:
- Schema validation in VolumeAttributeClass implementation in LVMVolume CR.
- Parameter boundary checking in CSI controller.
- Extensive logging of applied parameters with verification.

#### Risk 2: Performance Impact of Cgroup Operations
Frequent cgroup updates or misconfigured limits could impact overall node performance.

Mitigation:
- Batch cgroup operations where possible.

#### Risk 3: Version Compatibility Issues
In order for the CSI driver to work with the vac, a CSI sidecar update is required. Updating CSI sidecar containers may introduce compatibility issues with existing clusters.

Mitigation:
During development, I tested the update between minor versions, there were no problems. It should be mentioned in the documentation.

## Graduation Criteria
TODO

## Implementation History
The `Summary` and `Motivation` sections being merged signaling owner acceptance

## Testing
For testing, several stages must be implemented:
- Unit tests in the code that will cover possible options for configuring and parameterizing the vac.
- Integration tests that should show how the mounting itself and the application of parameters in /sys/fs/cgroup will work. There should be a scenario in which several pods are created with different numbers of replicas using volumeMode: Block and volumeMode: Filesystem, in which use different VAC.
