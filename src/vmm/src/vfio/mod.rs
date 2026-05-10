// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VFIO (Virtual Function I/O) passthrough support for Firecracker.
//!
//! This module implements the Linux VFIO type1 IOMMU path for PCI device passthrough.
//! All VFIO ioctls use `_IO(';', N)` format where ';' = 0x3b and N is the base+index.

pub mod container;
pub mod device;
pub mod group;
pub mod kvm_vfio;

pub use container::VfioContainer;
pub use device::VfioDevice;
pub use group::VfioGroup;
pub use kvm_vfio::KvmVfioDevice;

// VFIO ioctl numbers: _IO(';', VFIO_BASE + N) where VFIO_BASE = 100, ';' = 0x3b
// _IO(t, n) = (t << 8) | n, no size encoding (direction = 0)
pub const VFIO_GET_API_VERSION: u64 = 0x3b64; // _IO(';', 100)
pub const VFIO_CHECK_EXTENSION: u64 = 0x3b65; // _IO(';', 101)
pub const VFIO_SET_IOMMU: u64 = 0x3b66; // _IO(';', 102)

pub const VFIO_GROUP_GET_STATUS: u64 = 0x3b67; // _IO(';', 103)
pub const VFIO_GROUP_SET_CONTAINER: u64 = 0x3b68; // _IO(';', 104)
pub const VFIO_GROUP_UNSET_CONTAINER: u64 = 0x3b69; // _IO(';', 105)
pub const VFIO_GROUP_GET_DEVICE_FD: u64 = 0x3b6a; // _IO(';', 106)

pub const VFIO_DEVICE_GET_INFO: u64 = 0x3b6b; // _IO(';', 107)
pub const VFIO_DEVICE_GET_REGION_INFO: u64 = 0x3b6c; // _IO(';', 108)
pub const VFIO_DEVICE_GET_IRQ_INFO: u64 = 0x3b6d; // _IO(';', 109)
pub const VFIO_DEVICE_SET_IRQS: u64 = 0x3b6e; // _IO(';', 110)
pub const VFIO_DEVICE_RESET: u64 = 0x3b6f; // _IO(';', 111)

pub const VFIO_IOMMU_MAP_DMA: u64 = 0x3b71; // _IO(';', 113)
pub const VFIO_IOMMU_UNMAP_DMA: u64 = 0x3b72; // _IO(';', 114)

// VFIO API version
pub const VFIO_API_VERSION: i32 = 0;

// VFIO extension types
pub const VFIO_TYPE1_IOMMU: u64 = 1;

// VFIO group status flags
pub const VFIO_GROUP_FLAGS_VIABLE: u32 = 1 << 0;
pub const VFIO_GROUP_FLAGS_CONTAINER_SET: u32 = 1 << 1;

// VFIO device info flags
pub const VFIO_DEVICE_FLAGS_RESET: u32 = 1 << 0;
pub const VFIO_DEVICE_FLAGS_PCI: u32 = 1 << 1;

// VFIO region info flags
pub const VFIO_REGION_INFO_FLAG_READ: u32 = 1 << 0;
pub const VFIO_REGION_INFO_FLAG_WRITE: u32 = 1 << 1;
pub const VFIO_REGION_INFO_FLAG_MMAP: u32 = 1 << 2;

// VFIO PCI region indices
pub const VFIO_PCI_BAR0_REGION_INDEX: u32 = 0;
pub const VFIO_PCI_BAR5_REGION_INDEX: u32 = 5;
pub const VFIO_PCI_ROM_REGION_INDEX: u32 = 6;
pub const VFIO_PCI_CONFIG_REGION_INDEX: u32 = 7;

// VFIO PCI IRQ indices
pub const VFIO_PCI_MSI_IRQ_INDEX: u32 = 1;
pub const VFIO_PCI_MSIX_IRQ_INDEX: u32 = 2;

// VFIO IRQ info flags
pub const VFIO_IRQ_INFO_EVENTFD: u32 = 1 << 0;

// VFIO IRQ set flags
pub const VFIO_IRQ_SET_DATA_EVENTFD: u32 = 1 << 2;
pub const VFIO_IRQ_SET_ACTION_TRIGGER: u32 = 1 << 5;

// VFIO DMA map flags
pub const VFIO_DMA_MAP_FLAG_READ: u32 = 1 << 0;
pub const VFIO_DMA_MAP_FLAG_WRITE: u32 = 1 << 1;

// VFIO struct definitions matching Linux kernel uapi/linux/vfio.h

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VfioGroupStatus {
    pub argsz: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VfioDeviceInfo {
    pub argsz: u32,
    pub flags: u32,
    pub num_regions: u32,
    pub num_irqs: u32,
    pub cap_offset: u32,
    pub pad: u32,
}

/// Raw VFIO region info struct matching `struct vfio_region_info` from linux/vfio.h.
/// Used for the `VFIO_DEVICE_GET_REGION_INFO` ioctl and as storage in `VfioDevice`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VfioRegionInfo {
    pub argsz: u32,
    pub flags: u32,
    pub index: u32,
    pub cap_offset: u32,
    pub size: u64,
    pub offset: u64,
}

impl VfioRegionInfo {
    /// Whether this region can be mmap'd into host address space.
    pub fn is_mappable(&self) -> bool {
        self.flags & VFIO_REGION_INFO_FLAG_MMAP != 0
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VfioIrqInfo {
    pub argsz: u32,
    pub flags: u32,
    pub index: u32,
    pub count: u32,
}

/// Header for the vfio_irq_set struct (without the flexible data[] array).
/// The full struct is built as a Vec<u8> with eventfd values appended.
#[repr(C)]
#[derive(Debug)]
pub struct VfioIrqSetHeader {
    pub argsz: u32,
    pub flags: u32,
    pub index: u32,
    pub start: u32,
    pub count: u32,
}

#[repr(C)]
#[derive(Debug, Default)]
pub struct VfioIommuType1DmaMap {
    pub argsz: u32,
    pub flags: u32,
    pub vaddr: u64,
    pub iova: u64,
    pub size: u64,
}

/// Errors from VFIO operations.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum VfioError {
    /// VFIO API version mismatch: got {0}
    ApiVersionMismatch(i32),
    /// VFIO TYPE1 IOMMU not supported by kernel
    Type1IommuNotSupported,
    /// VFIO group is not viable (other devices in IOMMU group are unbound)
    GroupNotViable,
    /// IOMMU group isolation violated: device {0} is in group but not passed through
    IommuIsolationViolated(String),
    /// IO error: {0}
    Io(#[from] std::io::Error),
    /// VFIO device does not support reset (VFIO_DEVICE_FLAGS_RESET not set)
    ResetNotSupported,
    /// VFIO device is not a PCI device
    NotPciDevice,
    /// No BAR regions found on device
    NoBars,
    /// Failed to mmap BAR region {0}: size is zero
    BarSizeZero(u32),
    /// BAR region {0} does not support mmap
    BarNotMmappable(u32),
    /// KVM error: {0}
    Kvm(#[from] kvm_ioctls::Error),
    /// Resource allocation error: {0}
    ResourceAllocation(#[from] vm_allocator::Error),
    /// MSI-X interrupt error: {0}
    Interrupt(#[from] crate::vstate::interrupts::InterruptError),
    /// Could not find config region (VFIO_PCI_CONFIG_REGION_INDEX) on device
    NoConfigRegion,
    /// KVM memory slot exhausted
    NoKvmSlot,
}
