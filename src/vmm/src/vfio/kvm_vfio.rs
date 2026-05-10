// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::io::RawFd;

use kvm_bindings::{kvm_device_attr, kvm_device_type_KVM_DEV_TYPE_VFIO};
use kvm_ioctls::{DeviceFd, VmFd};

use super::VfioError;

// KVM VFIO device attribute group and attribute
const KVM_DEV_VFIO_GROUP: u32 = 1;
const KVM_DEV_VFIO_GROUP_ADD: u64 = 1;

/// Wraps the KVM VFIO device object (`KVM_DEV_TYPE_VFIO`).
///
/// The KVM VFIO device bridges VFIO groups to KVM's interrupt routing subsystem.
/// It must be created and a group added to it **before** calling
/// `VFIO_GROUP_GET_DEVICE_FD` (see M0 Section 1.9, critical ordering constraint).
#[derive(Debug)]
pub struct KvmVfioDevice {
    fd: DeviceFd,
}

impl KvmVfioDevice {
    /// Create a new KVM VFIO device object for the given VM file descriptor.
    pub fn new(vm_fd: &VmFd) -> Result<Self, VfioError> {
        let mut create_dev = kvm_bindings::kvm_create_device {
            type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
            fd: 0,
            flags: 0,
        };

        let device_fd = vm_fd
            .create_device(&mut create_dev)
            .map_err(VfioError::Kvm)?;

        Ok(KvmVfioDevice { fd: device_fd })
    }

    /// Add a VFIO group to this KVM VFIO device.
    ///
    /// This calls `KVM_SET_DEVICE_ATTR` with `KVM_DEV_VFIO_GROUP_ADD`, which triggers
    /// `vfio_file_set_kvm()` in the kernel — setting up the KVM↔VFIO interrupt routing link.
    ///
    /// **Must be called before `VfioGroup::get_device_fd()`.**
    pub fn add_group(&self, group_fd: RawFd) -> Result<(), VfioError> {
        let attr = kvm_device_attr {
            group: KVM_DEV_VFIO_GROUP,
            attr: KVM_DEV_VFIO_GROUP_ADD,
            // addr is the address of the group fd (a raw i32 on the stack)
            addr: &group_fd as *const RawFd as u64,
            flags: 0,
        };

        self.fd.set_device_attr(&attr).map_err(VfioError::Kvm)?;

        Ok(())
    }
}
