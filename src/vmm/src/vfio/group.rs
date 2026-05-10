// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use vmm_sys_util::ioctl::{ioctl_with_mut_ref, ioctl_with_ref};
use vmm_sys_util::syscall::SyscallReturnCode;

use super::{
    VFIO_GROUP_FLAGS_VIABLE, VFIO_GROUP_GET_DEVICE_FD, VFIO_GROUP_GET_STATUS,
    VFIO_GROUP_SET_CONTAINER, VfioError, VfioGroupStatus,
};
use super::container::VfioContainer;

/// Wraps a `/dev/vfio/<group_id>` group file descriptor.
///
/// A VFIO group represents an IOMMU group. All devices in the same IOMMU group must
/// either be bound to vfio-pci or have their drivers unbound before the group is viable.
#[derive(Debug)]
pub struct VfioGroup {
    fd: OwnedFd,
    iommu_group_id: u32,
}

impl VfioGroup {
    /// Create a new [`VfioGroup`] from an inherited fd.
    ///
    /// The caller is responsible for ensuring `fd` is a valid open file descriptor
    /// for `/dev/vfio/<iommu_group_id>`.
    pub fn new(fd: OwnedFd, iommu_group_id: u32) -> Self {
        VfioGroup { fd, iommu_group_id }
    }

    /// Check that the IOMMU group is viable (all devices are bound to vfio-pci or unbound).
    pub fn check_viable(&self) -> Result<(), VfioError> {
        let mut status = VfioGroupStatus {
            argsz: std::mem::size_of::<VfioGroupStatus>() as u32,
            flags: 0,
        };

        // SAFETY: fd is a valid VFIO group fd; status is a valid, fully initialised struct.
        SyscallReturnCode(unsafe {
            ioctl_with_mut_ref(&self.fd, VFIO_GROUP_GET_STATUS, &mut status)
        })
        .into_result()
        .map_err(VfioError::Io)?;

        if status.flags & VFIO_GROUP_FLAGS_VIABLE == 0 {
            return Err(VfioError::GroupNotViable);
        }

        Ok(())
    }

    /// Validate IOMMU group isolation by checking sysfs.
    ///
    /// All devices in `/sys/kernel/iommu_groups/<N>/devices/` must be either:
    /// (a) the device being passed through (`host_bdf`), or
    /// (b) bound to `vfio-pci` — meaning VFIO controls them and no native driver
    ///     has DMA access. This covers the common case of multi-function devices
    ///     (e.g. GPU + HDMI audio on the same BDF slot) where all functions share
    ///     an IOMMU group and all must be removed from native drivers.
    ///
    /// A device bound to any driver other than `vfio-pci` (or unbound but not the
    /// target) is rejected — it could issue DMA and break isolation.
    ///
    /// This is a hard gate per Decision 5 of the M0 design.
    pub fn validate_isolation(&self, host_bdf: &str) -> Result<(), VfioError> {
        let sysfs_path = format!(
            "/sys/kernel/iommu_groups/{}/devices",
            self.iommu_group_id
        );

        let entries = std::fs::read_dir(&sysfs_path).map_err(VfioError::Io)?;

        for entry in entries {
            let entry = entry.map_err(VfioError::Io)?;
            let name = entry.file_name();
            let device_bdf = name.to_string_lossy();

            // Strip domain prefix for comparison (sysfs uses "0000:bb:dd.f" form).
            let device_bdf_short = device_bdf.strip_prefix("0000:").unwrap_or(&device_bdf);
            let host_bdf_short = host_bdf.strip_prefix("0000:").unwrap_or(host_bdf);

            // The device being passed through is always acceptable.
            if device_bdf_short == host_bdf_short || device_bdf.as_ref() == host_bdf {
                continue;
            }

            // A companion device (e.g. HDMI audio on function 1 of a GPU) is acceptable
            // only if it is already under vfio-pci control.  Any other driver retains DMA
            // capability and would violate isolation.
            let driver_link = format!("/sys/bus/pci/devices/{}/driver", device_bdf);
            let bound_to_vfio = std::fs::read_link(&driver_link)
                .ok()
                .and_then(|p| p.file_name().map(|n| n == "vfio-pci"))
                .unwrap_or(false);

            if !bound_to_vfio {
                return Err(VfioError::IommuIsolationViolated(device_bdf.into_owned()));
            }
        }

        Ok(())
    }

    /// Attach this group to a VFIO container.
    ///
    /// Must be called before `VfioContainer::set_iommu()`.
    pub fn set_container(&self, container: &VfioContainer) -> Result<(), VfioError> {
        let container_fd = container.as_raw_fd();

        // SAFETY: fd is a valid VFIO group fd; container_fd is a valid VFIO container fd.
        // VFIO_GROUP_SET_CONTAINER takes a pointer to an int (the container fd).
        SyscallReturnCode(unsafe {
            ioctl_with_ref(&self.fd, VFIO_GROUP_SET_CONTAINER, &container_fd)
        })
        .into_result()
        .map_err(VfioError::Io)?;

        Ok(())
    }

    /// Get the device file descriptor for a PCI device in this group.
    ///
    /// `pci_addr` is the PCI BDF string, e.g. `"0000:03:00.0"`.
    ///
    /// The returned fd is owned by the caller.
    ///
    /// **Must be called AFTER `KvmVfioDevice::add_group()`** to ensure proper KVM integration
    /// (see Section 1.9 of the M0 design: critical ordering constraint).
    pub fn get_device_fd(&self, pci_addr: &str) -> Result<OwnedFd, VfioError> {
        let addr_cstr = CString::new(pci_addr)
            .map_err(|_| VfioError::Io(std::io::Error::other("PCI address contains nul byte")))?;

        // SAFETY: fd is a valid VFIO group fd; addr_cstr is a valid C string.
        // VFIO_GROUP_GET_DEVICE_FD returns a new file descriptor on success (positive int).
        let new_fd = SyscallReturnCode(unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                VFIO_GROUP_GET_DEVICE_FD as libc::c_ulong,
                addr_cstr.as_ptr(),
            )
        })
        .into_result()
        .map_err(VfioError::Io)?;

        // SAFETY: new_fd is a valid open file descriptor returned by the kernel.
        // We take ownership of it here; it will be closed when the OwnedFd is dropped.
        Ok(unsafe { OwnedFd::from_raw_fd(new_fd) })
    }

    /// Raw fd for use in `KVM_SET_DEVICE_ATTR` (KVM_DEV_VFIO_GROUP_ADD).
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
