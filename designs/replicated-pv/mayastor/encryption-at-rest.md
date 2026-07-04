---
oep-number: OEP 3843
title: OpenEBS Enhancement Proposal for Mayastor At-Rest Encryption of data
authors:
  - "@dsharma-dc"
owners:
  - "@dsharma-dc"
  - "@Abhinandan-Purkait"
editor: TBD
creation-date: 2025-01-24
last-updated: 2025-01-24
status: implemented
---

# OpenEBS Enhancement Proposal for Mayastor At-Rest Encryption of data

## Table of Contents

- [OpenEBS Enhancement Proposal for Mayastor At-Rest Encryption of data](#openebs-enhancement-proposal-for-mayastor-at-rest-encryption-of-data)
  - [Table of Contents](#table-of-contents)
  - [Overview](#overview)
  - [Motivation](#motivation)
  - [Goals](#goals)
  - [Non-Goals](#non-goals)
  - [Proposal](#proposal)
    - [Key Concepts](#key-concepts)
    - [Workflow](#workflow)
  - [User Stories](#user-stories)
  - [Implementation Details](#implementation-details)
    - [Design](#design)
    - [Components to Update](#components-to-update)
  - [Testing](#testing)

---

## Overview

This proposal introduces the encryption of data at-rest in the Replicated PV Mayastor (hereafter referred as Mayastor) diskpools. This feature ensures that:

1. All the data on a Maystor diskpool is stored encrypted if the encryption has been requested on the pool during pool creation.
2. It is not possible to disable encryption on a pool that has been created with encryption enabled initially.
3. The encryption can not be enabled on a pool that is already created un-encrypted.
4. A volume that is created with a topology for encrypted pools gets all the replicas placed on encrypted pools.

This feature ensures data protection and compliance for various use-cases of data management.

## Motivation

Implementing data encryption at rest ensures that sensitive information remains secure even if storage media is lost, stolen, or compromised. It protects against unauthorized access by ensuring data is only readable by authorized users or systems. Encryption also helps meet regulatory compliance requirements, demonstrating a commitment to privacy and data protection. Additionally, it mitigates the risk of data breaches, maintaining trust with stakeholders.

## Goals

- Implement the at-rest encryption of data in Mayastor, at a diskpool level so that all user data on the diskpool is stored encrypted.
- Support FIPS compliant encryption methods.

## Non-Goals

- This proposal does not implement (or integrate with) any Key management service for the encryption keys.
- This proposal does not implement in-flight data encryption.
- This proposal does not implement support for disabling encryption on a pool that has been created with encryption.
- This proposal does not implement support for enabling encryption on a pool that is already created as un-encrypted.
- This proposal does not implement or strengthen any existing communication protocols between various services.

## Proposal

### Key Concepts

1. **Diskpool Level Encryption**: Ability to encrypt data at a pool level without differentiating which replica the data belongs to. All replicas' data on such a pool will get encrypted.
2. **Encryption Key Management**: This is a prerequisite.
   - A key is provisioned by admin or a Key Management Service (KMS) with a supported cipher, for the use of storage system, and stored as a Kubernetes Secret.
   - The admin can additionally safeguard the key via Resource-Encryption-at-Rest facility provided by Kubernetes.

### Workflow

1. **Encrypted Diskpool Creation**:
   - The user/admin creates a diskpool yaml spec that contains the name of the Secret which holds the key.
   - When the spec is applied, the diskpool operator picks up this request and dispatches the create pool request containing Secret name to the Mayastor agents that complete the diskpool provisioning.
   - The spec for creating a pool with encryption will look like below:

```yaml
apiVersion: "openebs.io/v1beta3"
kind: DiskPool
metadata:
  name: <pool-name>
  namespace: <namespace>
spec:
  node: <node-name>
  disks: ["/dev/disk/by-id/<id>"]
  encryptionConfig:
    source:
      secret:
        name: <myKeySecretName>
```

2. **Diskpool CRD migration**:
   - After an upgrade to Diskpool CRD version v1beta3 happens in the cluster, any existing CRs that are on version v1beta2 will have to be migrated to version v1beta3 by the implementation. This would also require that the new field `encryptionConfig` be defined as optional in the CRD.

3. **Diskpool import upon node or io-engine restart**:
   - In the event of a node restart, io-engine restart or a node going offline and coming up again - the diskpool
   is imported. The import needs to be done using the same DEK that was used during diskpool creation.

4. **Volume Provisioning**:
   - Pool topology rules ensure that for a volume requesting encryption, the replicas are only placed on the diskpools that have
   encryption enabled on them. This will be handled by storageclass via the poolHasTopologyKey setting to let the volume replica placement happen on pools that are labelled with a specific key identifying encryption.
   - The storageclass definition required for volumes to be encrypted will have an additional field named `encrypted`, which needs to be set to `true` if encryption is required.

```yaml
kind: StorageClass
apiVersion: storage.k8s.io/v1
metadata:
  name: mayastor-3
parameters:
  encrypted: "true"
  repl: "3"
provisioner: io.openebs.csi-mayastor
```

## User Stories

1. **Story 1**: As an organisation's security lead, I want all our data getting stored on the storage systems be encrypted with a key that our admins provide.
2. **Story 2**: As a system administrator, I want minimal configuration control to easily let storage use the encryption keys for data at-rest encryption.

## Implementation Details

### Design

- **Encryption Key Loading Options**:
  - Pool create/import supports two ways to provide encryption parameters:
  - Option 1 (`EncryptionData`): pass raw key material (cipher + key data) over gRPC in the pool request.
  - Option 2 (`EncryptionSecret`): pass a Secret source name over gRPC and let io-engine resolve the Kubernetes Secret and parse the DEK.

- **Current Decision (Secret Name for now)**:
  - Add a new field `encryptionConfig` to the diskpool CR. This field holds the source metadata for the Secret object that contains the DEK.
  - The control plane agent-core forwards only the Secret reference to io-engine and does not load or parse the DEK.
  - The gRPC channel from agent-core to io-engine is currently not TLS-protected.
  - Because of that, this implementation does not send raw key material over that gRPC path.
  - io-engine resolves the Kubernetes Secret and parses the actual Data Encryption Key (DEK) during pool create/import.
  - Once the pool is created using a Secret, the key for that pool can't be transparently changed via a different Secret. Doing so will require a full rebuild of pool onto a different pool.

- **Secret Name during Pool Import**:
  - Set the `encryptionConfig` in the PoolSpec.
  - When a pool import is required, io-engine uses the `encryptionConfig` from PoolSpec to fetch the DEK again.
  - Dispatch the import operation to data plane.

- **PoolCreateRequest API Change**:
  - Pool create/import encryption parameters are passed either as raw encryption data or as a Secret source reference.

```protobuf
// Encryption parameters for this pool. Either as raw key params, OR a name to a Kubernetes
// Secret resource or a file.
oneof encryption {
  EncryptionData data = 7;
  EncryptionSecret secret = 8;
}

// Represents an encryption key that can be used to encrypt an
// entity like pool or lvol/replica.
message EncryptionKey {
  // Name of the key.
  string key_name = 1;
  // The AES encryption key.
  bytes key = 2;
  // AES Key length.
  uint32 key_length = 3;
  // key2 (required for AES_XTS).
  optional bytes key2 = 4;
  // The length of key2. Must be same as key_length.
  optional uint32 key2_length = 5;
}

message EncryptionData {
  // Cipher to be used.
  Cipher cipher = 1;
  // The encryption key.
  EncryptionKey key = 2;
}

// This message represents name of the source for getting
// key parameter details.
message EncryptionSecret {
  string secret = 1;
}
```

### Components to Update

- **Diskpool Custom Resource Definition**: The Custom Resource needs to identify the `encryptionConfig` field.
- **Control-Plane agent-core**: The agent needs to pass the Secret source reference to io-engine as part of the pool request.
- **Data-Plane io-engine**: io-engine needs to resolve Kubernetes Secret references, parse encryption parameters, and create/place a crypto block device on top of base block device of the diskpool.

## Testing

- Create a diskpool with a Secret of AES_CBC cipher and 128-bit key. The pool creation must succeed.
- Create a diskpool with a Secret of AES_CBC cipher and 256-bit key. The pool creation must succeed.
- Create a diskpool with a Secret of AES_XTS cipher and 128-bit keys. The pool creation must succeed.
- Upon a node restart, the import of the encrypted pool on that node must successfully complete.
- Provision a volume via encryption storage class. The volume replicas must get placed only on encrypted pools.
- Scale up an encrypted volume. The new replicas must get placed only on encrypted pools.
