// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// M1 experimental API shape for attaching a VFIO PCI device.
///
/// The caller (typically the jailer) opens `/dev/vfio/vfio` and
/// `/dev/vfio/<iommu_group_id>` before chroot, then passes the inherited fd
/// numbers in this structure. The fd numbers are only meaningful within the
/// Firecracker process's fd namespace.
///
/// See M0 Section 6 for the long-term API redesign target.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct VfioDeviceConfig {
    /// PCI BDF of the host device, e.g. `"0000:03:00.0"`
    pub host_bdf: String,
    /// File descriptor number of the pre-inherited `/dev/vfio/vfio` container
    pub container_fd: i32,
    /// File descriptor number of the pre-inherited `/dev/vfio/<group_id>` group
    pub group_fd: i32,
    /// IOMMU group ID, used for sysfs isolation check
    pub iommu_group_id: u32,
    /// Optional desired guest BDF, e.g. `"00:02.0"`. Allocated automatically if absent.
    pub guest_bdf: Option<String>,
}

/// Errors when configuring a VFIO device.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioConfigError {
    /// VFIO is only supported with PCI enabled (pass --enable-pci)
    PciNotEnabled,
    /// Cannot attach VFIO device when a balloon device is present (DMA safety violation)
    BalloonConflict,
    /// Cannot attach balloon device when VFIO devices are present (DMA safety violation)
    VfioConflictWithBalloon,
    /// VFIO device attach error: {0}
    VfioError(#[from] crate::vfio::VfioError),
}
