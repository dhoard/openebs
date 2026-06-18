---
oep-number: OEP 4076
title: LocalPV-LVM tag support
authors:
  - "@jochenseeber"
owners: []
editor: []
creation-date: 2025-10-07
last-updated: 2025-10-07
status: provisional
---

# Native LVM Tags

## Table of Contents

* [Native LVM Tags](#native-lvm-tags)
  * [Table of Contents](#table-of-contents)
  * [Summary](#summary)
  * [Motivation](#motivation)
    * [Goals](#goals)
    * [Non\-Goals](#non-goals)
  * [Proposal](#proposal)
    * [User Stories](#user-stories)
      * [Setting tags using the storage class](#setting-tags-using-the-storage-class)
      * [Setting tags using the persistent volume claim](#setting-tags-using-the-persistent-volume-claim)
    * [Implementation Details/Notes/Constraints](#implementation-detailsnotesconstraints)
  * [Testing](#testing)
  * [Graduation Criteria](#graduation-criteria)

## Summary

This proposal suggests supporting native LVM tags for Logical Volumes (LVs) created by lvm-localpv. Tags are strings that are attached to the LV and can be used to identify LVs for LVM commands, LVM configuration, or other commands.

For example, LVs could be tagged with their environment (`env=prod` or `env=test`), by their required backup strategy (`backup=daily` or `backup=weekly`). Admins could then use these tags to target a group of LVs in one go.

## Motivation

From the LVM man page:

> Tags are user-defined strings that can be attached to PVs, VGs and LVs. Tags can be displayed with commands `pvs/vgs/lvs -o tags`. Certain commands will accept a tag name in place of a PV, VG, or LV name. In these cases, the command will operate on each PV/VG/LV with the given tag. Tags should be prefixed with `@` to avoid ambiguity.
>
> Characters allowed in tags are: `A–Z a–z 0–9 _ + . - / = ! : # &`

Tags would greatly simplify admin work by allowing admins to target groups of related LVs (e.g. all production LVs) instead of individual LVs. For example:

* Configuration Management: LVM's configuration file can use tags to apply specific settings to groups of volumes, allowing for fine-grained control over the storage infrastructure.
* Selective Backups: An admin could easily script a backup routine that targets all volumes with the tag `backup=daily`.
* Targeted LVM Operations: When performing maintenance, an admin could use LVM commands to check all logical volumes tagged as `env=dev` in a single operation.

### Goals

* Allow the user to set and add tags to created LVs
* Specify tags in the storage class for tags that depend on the storage class
* Specify tags in the PVC's volume attribute class for tags that depend on the individual request

### Non-Goals

* Removing tags

## Proposal

### User Stories

#### Setting tags using the storage class

* As a user, I can specify in the storage class which tags should be added to any created LV

```yaml
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: production-database
allowVolumeExpansion: true
parameters:
  volgroup: lvmvg
  tags: env=prod,backup=never # Just checking if you're paying attention ;-)
provisioner: local.csi.openebs.io
```

#### Setting tags using the persistent volume claim

* As a user, I can specify in the persistent volume claim which tags should be added to any created LV
* As a user, I can update the tags parameter in the storage class, and new tags will be applied

```yaml
---
apiVersion: storage.k8s.io/v1
kind: VolumeAttributesClass
metadata:
  name: my-database
driverName: local.csi.openebs.io
parameters:
  tags: app=myapp,nomonitor
---
kind: PersistentVolumeClaim
apiVersion: v1
metadata:
  name: my-database
spec:
  storageClassName: production-database
  volumeAttributesClassName: my-database
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 4Gi

```

### Implementation Details/Notes/Constraints

* When creating a LV, it is tagged with the union of all tags specified in the storage class and in the PVC's VolumeAttributeClass

## Testing

Integration tests required for:

* Creating a volume with tags from the SC and the PVC
* Adding tags to existing volumes by changing the the PVC

## Graduation Criteria

* Integration tests work