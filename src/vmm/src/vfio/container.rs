// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use vmm_sys_util::ioctl::{ioctl, ioctl_with_mut_ref, ioctl_with_val};
use vmm_sys_util::syscall::SyscallReturnCode;

use super::{
    VFIO_API_VERSION, VFIO_CHECK_EXTENSION, VFIO_DMA_MAP_FLAG_READ, VFIO_DMA_MAP_FLAG_WRITE,
    VFIO_GET_API_VERSION, VFIO_IOMMU_MAP_DMA, VFIO_SET_IOMMU, VFIO_TYPE1_IOMMU,
    VfioError, VfioIommuType1DmaMap,
};

/// Wraps a `/dev/vfio/vfio` container file descriptor.
///
/// The container aggregates VFIO groups under a shared IOMMU domain.
/// It is opened by the jailer before chroot and inherited as an fd.
#[derive(Debug)]
pub struct VfioContainer {
    fd: OwnedFd,
}

impl VfioContainer {
    /// Create a new [`VfioContainer`] from an inherited fd.
    ///
    /// Verifies the VFIO API version and that TYPE1 IOMMU is supported.
    ///
    /// # Safety note
    /// The caller must ensure `fd` is a valid open file descriptor for `/dev/vfio/vfio`
    /// and that ownership is transferred here (it will be closed on drop).
    pub fn new(fd: OwnedFd) -> Result<Self, VfioError> {
        let container = VfioContainer { fd };

        // Verify API version
        // SAFETY: fd is a valid VFIO container fd; VFIO_GET_API_VERSION takes no argument
        // and returns the API version as an integer.
        let version = SyscallReturnCode(unsafe { ioctl(&container.fd, VFIO_GET_API_VERSION) })
            .into_result()
            .map_err(VfioError::Io)?;

        if version != VFIO_API_VERSION {
            return Err(VfioError::ApiVersionMismatch(version));
        }

        // Verify TYPE1 IOMMU extension is available
        // SAFETY: fd is a valid VFIO container fd; VFIO_CHECK_EXTENSION takes an extension
        // type as a c_ulong and returns non-zero if supported.
        let ext_ok = SyscallReturnCode(unsafe {
            ioctl_with_val(&container.fd, VFIO_CHECK_EXTENSION, VFIO_TYPE1_IOMMU)
        })
        .into_result()
        .map_err(VfioError::Io)?;

        if ext_ok == 0 {
            return Err(VfioError::Type1IommuNotSupported);
        }

        Ok(container)
    }

    /// Set the IOMMU type for this container to TYPE1.
    ///
    /// Must be called after at least one group has been attached to the container
    /// via `VFIO_GROUP_SET_CONTAINER`.
    pub fn set_iommu(&self) -> Result<(), VfioError> {
        // SAFETY: fd is a valid VFIO container fd with at least one group attached.
        // VFIO_SET_IOMMU takes the IOMMU type as a c_ulong.
        SyscallReturnCode(unsafe {
            ioctl_with_val(&self.fd, VFIO_SET_IOMMU, VFIO_TYPE1_IOMMU)
        })
        .into_result()
        .map_err(VfioError::Io)?;

        Ok(())
    }

    /// Map a guest RAM region into the IOMMU so the passthrough device can DMA to it.
    ///
    /// # Arguments
    /// * `vaddr` - Host virtual address of the guest RAM region
    /// * `iova` - Guest physical address (IOVA) to map
    /// * `size` - Size in bytes of the region
    pub fn map_dma(&self, vaddr: u64, iova: u64, size: u64) -> Result<(), VfioError> {
        let mut dma_map = VfioIommuType1DmaMap {
            argsz: std::mem::size_of::<VfioIommuType1DmaMap>() as u32,
            flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
            vaddr,
            iova,
            size,
        };

        // SAFETY: fd is a valid VFIO container fd with IOMMU type set.
        // dma_map is a valid, fully initialised struct on the stack.
        SyscallReturnCode(unsafe {
            ioctl_with_mut_ref(&self.fd, VFIO_IOMMU_MAP_DMA, &mut dma_map)
        })
        .into_result()
        .map_err(VfioError::Io)?;

        Ok(())
    }

    /// Raw fd for use in `VFIO_GROUP_SET_CONTAINER`.
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
