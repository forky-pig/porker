// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::io::RawFd;

use vmm_sys_util::ioctl::{ioctl, ioctl_with_mut_ref};
use vmm_sys_util::syscall::SyscallReturnCode;

use super::{
    VFIO_DEVICE_FLAGS_PCI, VFIO_DEVICE_FLAGS_RESET, VFIO_DEVICE_GET_INFO,
    VFIO_DEVICE_GET_IRQ_INFO, VFIO_DEVICE_GET_REGION_INFO, VFIO_DEVICE_RESET,
    VFIO_DEVICE_SET_IRQS, VFIO_IRQ_SET_ACTION_TRIGGER, VFIO_IRQ_SET_DATA_EVENTFD,
    VFIO_PCI_BAR0_REGION_INDEX, VFIO_PCI_BAR5_REGION_INDEX, VFIO_PCI_CONFIG_REGION_INDEX,
    VfioDeviceInfo, VfioError, VfioIrqInfo, VfioIrqSetHeader, VfioRegionInfo,
};

/// Wraps a VFIO device file descriptor (obtained via `VFIO_GROUP_GET_DEVICE_FD`).
#[derive(Debug)]
pub struct VfioDevice {
    pub(crate) fd: OwnedFd,
    pub info: VfioDeviceInfo,
    pub regions: Vec<VfioRegionInfo>,
}

impl VfioDevice {
    /// Create a new [`VfioDevice`] from a device fd, querying device and region info.
    pub fn new(fd: OwnedFd) -> Result<Self, VfioError> {
        let mut info = VfioDeviceInfo {
            argsz: std::mem::size_of::<VfioDeviceInfo>() as u32,
            ..Default::default()
        };

        // SAFETY: fd is a valid VFIO device fd obtained from VFIO_GROUP_GET_DEVICE_FD.
        // info is fully initialised with argsz set correctly for the kernel to validate.
        SyscallReturnCode(unsafe { ioctl_with_mut_ref(&fd, VFIO_DEVICE_GET_INFO, &mut info) })
            .into_result()
            .map_err(VfioError::Io)?;

        if info.flags & VFIO_DEVICE_FLAGS_PCI == 0 {
            return Err(VfioError::NotPciDevice);
        }

        let mut device = VfioDevice {
            fd,
            info,
            regions: Vec::new(),
        };

        device.populate_regions()?;

        Ok(device)
    }

    /// Whether this device supports FLR/reset via `VFIO_DEVICE_RESET`.
    pub fn supports_reset(&self) -> bool {
        self.info.flags & VFIO_DEVICE_FLAGS_RESET != 0
    }

    /// Issue a device reset.
    pub fn reset(&self) -> Result<(), VfioError> {
        // SAFETY: fd is a valid VFIO device fd; VFIO_DEVICE_RESET takes no argument.
        SyscallReturnCode(unsafe { ioctl(&self.fd, VFIO_DEVICE_RESET) })
            .into_result()
            .map_err(VfioError::Io)?;
        Ok(())
    }

    /// Query region info for all regions reported by the device.
    fn populate_regions(&mut self) -> Result<(), VfioError> {
        let num_regions = self.info.num_regions;

        for i in 0..num_regions {
            let mut region_info = VfioRegionInfo {
                argsz: std::mem::size_of::<VfioRegionInfo>() as u32,
                index: i,
                ..Default::default()
            };

            // SAFETY: fd is valid; region_info is a valid, fully initialised struct with
            // argsz and index set. The kernel fills in the remaining fields.
            let ret = unsafe {
                ioctl_with_mut_ref(&self.fd, VFIO_DEVICE_GET_REGION_INFO, &mut region_info)
            };

            if ret < 0 {
                // Region doesn't exist on this device (e.g. VFIO_PCI_VGA_REGION_INDEX on a
                // non-VGA device). The kernel reports num_regions=9 for all PCI devices but
                // returns EINVAL for optional regions the device doesn't implement. Skip it.
                continue;
            }

            if region_info.size > 0 {
                self.regions.push(region_info);
            }
        }

        Ok(())
    }

    /// Get IRQ info for a given IRQ index (e.g., `VFIO_PCI_MSIX_IRQ_INDEX`).
    pub fn get_irq_info(&self, irq_index: u32) -> Result<VfioIrqInfo, VfioError> {
        let mut irq_info = VfioIrqInfo {
            argsz: std::mem::size_of::<VfioIrqInfo>() as u32,
            index: irq_index,
            ..Default::default()
        };

        // SAFETY: fd is valid; irq_info is a valid, fully initialised struct.
        SyscallReturnCode(unsafe {
            ioctl_with_mut_ref(&self.fd, VFIO_DEVICE_GET_IRQ_INFO, &mut irq_info)
        })
        .into_result()
        .map_err(VfioError::Io)?;

        Ok(irq_info)
    }

    /// Wire MSI-X interrupt vectors to eventfds via `VFIO_DEVICE_SET_IRQS`.
    ///
    /// `irq_index` is `VFIO_PCI_MSIX_IRQ_INDEX`.
    /// `start` is the first vector index.
    /// `fds` contains the raw eventfd for each vector.
    pub fn set_irqs_eventfds(
        &self,
        irq_index: u32,
        start: u32,
        fds: &[RawFd],
    ) -> Result<(), VfioError> {
        let count = fds.len() as u32;

        // Build the variable-length vfio_irq_set buffer:
        //   VfioIrqSetHeader (5 × u32 = 20 bytes) + data[] (count × i32 = 4 bytes each)
        let header_size = std::mem::size_of::<VfioIrqSetHeader>();
        let data_size = fds.len() * std::mem::size_of::<i32>();
        let total_size = header_size + data_size;
        let argsz = total_size as u32;

        let mut buf = vec![0u8; total_size];

        let header = VfioIrqSetHeader {
            argsz,
            flags: VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER,
            index: irq_index,
            start,
            count,
        };

        // SAFETY: buf has exactly header_size bytes at buf.as_mut_ptr();
        // VfioIrqSetHeader is repr(C) with no implicit padding.
        unsafe {
            std::ptr::copy_nonoverlapping(
                &header as *const VfioIrqSetHeader as *const u8,
                buf.as_mut_ptr(),
                header_size,
            );
        }

        // Append the eventfd array as little-endian i32 values
        for (i, &fd) in fds.iter().enumerate() {
            let offset = header_size + i * std::mem::size_of::<i32>();
            buf[offset..offset + 4].copy_from_slice(&fd.to_ne_bytes());
        }

        // SAFETY: self.fd is a valid VFIO device fd; buf is a valid buffer whose layout
        // matches struct vfio_irq_set with trailing data[count] of i32 file descriptors.
        SyscallReturnCode(unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                VFIO_DEVICE_SET_IRQS as libc::c_ulong,
                buf.as_ptr(),
            )
        })
        .into_result()
        .map_err(VfioError::Io)?;

        Ok(())
    }

    /// mmap a BAR region from the device fd into host address space.
    ///
    /// Returns a raw pointer to the mmap'd region. The caller is responsible for
    /// calling `libc::munmap(ptr as *mut _, size)` when done with the mapping.
    pub fn mmap_bar(&self, region: &VfioRegionInfo) -> Result<*mut u8, VfioError> {
        if region.size == 0 {
            return Err(VfioError::BarSizeZero(region.index));
        }

        if !region.is_mappable() {
            return Err(VfioError::BarNotMmappable(region.index));
        }

        // SAFETY: The mmap parameters are valid: fd is open and valid; the offset is
        // the VFIO region offset (aligned by the kernel); size is non-zero.
        // MAP_SHARED means writes reach the device MMIO registers through the IOMMU.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                region.size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd.as_raw_fd(),
                region.offset as libc::off_t,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(VfioError::Io(std::io::Error::last_os_error()));
        }

        Ok(ptr as *mut u8)
    }

    /// Read bytes from the device config space at byte `offset` within the config region.
    pub fn read_config_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), VfioError> {
        let region = self.config_region().ok_or(VfioError::NoConfigRegion)?;
        let file_offset = region.offset + offset;

        // SAFETY: fd is valid; buf is a valid mutable slice; file_offset lies within the
        // config region reported by VFIO_DEVICE_GET_REGION_INFO.
        let ret = unsafe {
            libc::pread(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                file_offset as libc::off_t,
            )
        };

        if ret < 0 {
            return Err(VfioError::Io(std::io::Error::last_os_error()));
        }

        if ret as usize != buf.len() {
            return Err(VfioError::Io(std::io::Error::other(
                "short read on VFIO config region",
            )));
        }

        Ok(())
    }

    /// Write bytes to the device config space at byte `offset` within the config region.
    pub fn write_config_at(&self, offset: u64, buf: &[u8]) -> Result<(), VfioError> {
        let region = self.config_region().ok_or(VfioError::NoConfigRegion)?;
        let file_offset = region.offset + offset;

        // SAFETY: fd is valid; buf is a valid slice; file_offset is within the config region.
        let ret = unsafe {
            libc::pwrite(
                self.fd.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                file_offset as libc::off_t,
            )
        };

        if ret < 0 {
            return Err(VfioError::Io(std::io::Error::last_os_error()));
        }

        Ok(())
    }

    /// Find the config space region (index `VFIO_PCI_CONFIG_REGION_INDEX`).
    pub fn config_region(&self) -> Option<&VfioRegionInfo> {
        self.regions
            .iter()
            .find(|r| r.index == VFIO_PCI_CONFIG_REGION_INDEX)
    }

    /// Find the expansion ROM region (index `VFIO_PCI_ROM_REGION_INDEX`), if present.
    pub fn rom_region(&self) -> Option<&VfioRegionInfo> {
        self.regions
            .iter()
            .find(|r| r.index == super::VFIO_PCI_ROM_REGION_INDEX)
    }

    /// Iterator over BAR regions (indices 0–5) that the device exposes.
    pub fn bar_regions(&self) -> impl Iterator<Item = &VfioRegionInfo> {
        self.regions.iter().filter(|r| {
            r.index >= VFIO_PCI_BAR0_REGION_INDEX && r.index <= VFIO_PCI_BAR5_REGION_INDEX
        })
    }
}
