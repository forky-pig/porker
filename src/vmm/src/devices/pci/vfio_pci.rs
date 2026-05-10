// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! VFIO PCI passthrough device implementing the [`PciDevice`] trait.
//!
//! This device proxies PCI configuration space accesses to the underlying hardware
//! (via pread/pwrite on the VFIO device fd), intercepts BAR registers to return
//! guest-allocated MMIO addresses, and manages MSI-X interrupt routing.

use std::os::fd::AsRawFd;
use std::sync::{Arc, Barrier};

use kvm_bindings::kvm_userspace_memory_region;
use vm_memory::GuestMemoryRegion;

use crate::pci::{DeviceRelocationError, PciDevice};
use crate::pci::PciSBDF;
use crate::vfio::{VfioContainer, VfioDevice, VfioError, VfioGroup, KvmVfioDevice};
use crate::vfio::{
    VFIO_PCI_MSI_IRQ_INDEX, VFIO_PCI_MSIX_IRQ_INDEX, VFIO_IRQ_INFO_EVENTFD,
    VFIO_PCI_ROM_REGION_INDEX,
};
use crate::vstate::interrupts::{InterruptError, MsixVector, MsixVectorConfig, MsixVectorGroup};
use crate::Vm;

// PCI config space register indices (each register is 4 bytes)
const PCI_BAR0_REG_IDX: u16 = 4;  // offset 0x10
const PCI_BAR5_REG_IDX: u16 = 9;  // offset 0x24
const PCI_ROM_REG_IDX: u16 = 12;   // offset 0x30 — expansion ROM BAR

// PCI config space byte offsets
const PCI_STATUS_OFFSET: u64 = 0x06;
const PCI_CAP_POINTER_OFFSET: u64 = 0x34;
const PCI_BAR0_OFFSET: u64 = 0x10;

// PCI capability IDs
const PCI_CAP_ID_MSI: u8 = 0x05;
const PCI_CAP_ID_MSIX: u8 = 0x11;

// MSI capability field offsets (relative to cap start)
const MSI_CAP_CONTROL_OFFSET: u8 = 2; // 2-byte message control field
const MSI_CTRL_ENABLE: u16 = 1 << 0; // bit 0 of message control
const MSI_CTRL_64BIT: u16 = 1 << 7; // bit 7: 64-bit address capable
// Byte offsets of addr/data within the MSI capability (after the 4-byte header)
const MSI_CAP_ADDR_LO_OFFSET: u64 = 4;
const MSI_CAP_ADDR_HI_OFFSET: u64 = 8; // only present when 64-bit
const MSI_CAP_DATA_32_OFFSET: u64 = 8; // data offset for 32-bit MSI
const MSI_CAP_DATA_64_OFFSET: u64 = 12; // data offset for 64-bit MSI

// MSI-X capability field offsets (relative to cap start)
const MSIX_CAP_CONTROL_OFFSET: u8 = 2; // 2-byte field
const MSIX_CAP_TABLE_OFFSET: u8 = 4; // 4-byte field: [2:0]=BIR, [31:3]=offset>>3

// MSI-X control register bits
const MSIX_CTRL_ENABLE: u16 = 1 << 15;

// MSI-X table entry size in bytes
const MSIX_ENTRY_SIZE: usize = 16; // 4 × u32: addr_lo, addr_hi, data, vector_ctrl
const MSIX_ENTRY_VECTOR_CTRL_MASKED: u32 = 1;

// BAR type bits (lower 4 bits of BAR register value)
const PCI_BAR_IO: u32 = 1;
const PCI_BAR_MEM_TYPE_MASK: u32 = 0b110; // bits [2:1]
const PCI_BAR_MEM_TYPE_64BIT: u32 = 0b100; // bits [2:1] = 10

/// Represents a single mapped PCI BAR region.
struct VfioBar {
    /// VFIO region index (0 = BAR0, 1 = BAR1, …, 5 = BAR5)
    region_index: u32,
    /// Lower 4 bits of the hardware BAR register (type and prefetchable flags)
    type_bits: u32,
    /// True if this is a 64-bit BAR (uses this region + the next PCI BAR register slot)
    is_64bit: bool,
    /// Guest physical address where this BAR is mapped
    guest_addr: u64,
    /// Size in bytes of this BAR
    size: u64,
    /// Host virtual address of the mmap'd BAR, or null if not mmap'd
    host_ptr: *mut u8,
    /// KVM memory slot number for this BAR (if host_ptr is non-null)
    kvm_slot: u32,
}

// SAFETY: Raw pointer is only accessed from the owning thread or through the Vm lock.
// VfioPciDevice is accessed under the PCI bus mutex in Firecracker, so accesses are serialised.
unsafe impl Send for VfioBar {}

/// VFIO PCI passthrough device.
///
/// Connects a host PCI device to the Firecracker guest via:
/// - VFIO BAR mmap → KVM memory slots (for hardware-accessible MMIO)
/// - VFIO eventfds → KVM irqfds (for MSI-X interrupt delivery)
/// - Config space proxy via pread/pwrite on the VFIO device fd
pub struct VfioPciDevice {
    /// VFIO device handle (owns the device_fd)
    device: VfioDevice,
    /// VFIO container — kept alive for the lifetime of this device
    _container: VfioContainer,
    /// VFIO group — kept alive for the lifetime of this device
    _group: VfioGroup,
    /// KVM VFIO device — kept alive for the lifetime of this device
    _kvm_vfio: KvmVfioDevice,
    /// Guest SBDF assigned to this device
    pub sbdf: PciSBDF,
    /// Mapped BAR regions
    bars: Vec<VfioBar>,
    /// MSI-X interrupt vectors
    msix_vectors: MsixVectorGroup,
    /// Reference to the VM (for KVM irqfd/routing operations)
    vm: Arc<Vm>,
    /// Byte offset of the MSI-X capability in config space (if present)
    msix_cap_offset: Option<u32>,
    /// Which VFIO region index contains the MSI-X table (from Table BIR)
    msix_table_bar_index: Option<u32>,
    /// Byte offset of the MSI-X table within its BAR
    msix_table_offset: u64,
    /// Number of MSI-X vectors the device supports
    num_msix_vectors: u32,
    /// MSI capability offset and single vector, for devices that have MSI but not MSI-X.
    msi_cap_offset: Option<u32>,
    /// True if the MSI capability uses 64-bit address fields.
    msi_is_64bit: bool,
    /// Allocated GSI + eventfd for MSI interrupt delivery (None if device uses MSI-X).
    msi_vector: Option<MsixVectorGroup>,
    /// Per-BAR-slot probe state: true when the kernel has written 0xFFFF_FFFF to the slot
    /// and the next read should return the size mask instead of the guest address.
    /// Indexed by BAR slot (0 = BAR0 reg, 1 = BAR1 reg, …, 5 = BAR5 reg).
    bar_probe: [bool; 6],
    /// Expansion ROM mapping (VFIO region 6), if the device exposes one and it is mappable.
    rom_bar: Option<VfioBar>,
    /// True while the guest is probing the ROM BAR size (wrote all-ones to reg 12).
    rom_probe: bool,
    /// Cached value of the ROM enable bit (bit 0 of the ROM BAR register).
    rom_enable: bool,
}

impl std::fmt::Debug for VfioPciDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VfioPciDevice")
            .field("sbdf", &self.sbdf)
            .field("num_msix_vectors", &self.num_msix_vectors)
            .finish()
    }
}

impl VfioPciDevice {
    /// Construct a new [`VfioPciDevice`].
    ///
    /// Runs the complete VFIO attach sequence (phases C–H from M0 Section 3):
    /// group viability check → isolation check → container attach → IOMMU type →
    /// KVM VFIO device → device fd → BAR map → IOMMU DMA map → MSI-X eventfd wire-up.
    ///
    /// # Arguments
    /// * `container` - Pre-opened `/dev/vfio/vfio` container
    /// * `group` - Pre-opened `/dev/vfio/<group_id>` group
    /// * `host_bdf` - PCI BDF string of the host device, e.g. `"0000:03:00.0"`
    /// * `vm` - Reference to the VM (for KVM slots, GSI allocation, etc.)
    /// * `sbdf` - Guest SBDF to assign
    pub fn new(
        container: VfioContainer,
        group: VfioGroup,
        host_bdf: &str,
        vm: Arc<Vm>,
        sbdf: PciSBDF,
    ) -> Result<Self, VfioError> {
        // Phase C step 8: Create KVM VFIO device
        let kvm_vfio = KvmVfioDevice::new(&vm.fd())?;

        // Phase C step 9: KVM_DEV_VFIO_GROUP_ADD BEFORE VFIO_GROUP_GET_DEVICE_FD
        kvm_vfio.add_group(group.as_raw_fd())?;

        // Phase D step 10: Get device fd (open_device() fires here; reads vfio_device->kvm)
        let device_fd = group.get_device_fd(host_bdf)?;

        // Phase E steps 11-12: Query device and region info
        let device = VfioDevice::new(device_fd)?;

        if !device.supports_reset() {
            return Err(VfioError::ResetNotSupported);
        }

        // FLR the device before the guest driver takes over. The NIC may have been
        // left in mid-operation state by the host (e.g. TX/RX FIFOs active, PHY
        // in an unknown negotiation state). A clean reset here ensures the guest
        // driver sees a power-on device state.
        device.reset()?;

        // Phase F steps 14-16: Map BARs into guest address space
        let bars = Self::map_bars(&device, &vm)?;

        // Map expansion ROM (VFIO region 6) if the device has one.
        let rom_bar = Self::map_rom_bar(&device, &vm)?;

        // Phase G step 17: Map all guest RAM regions into the IOMMU
        Self::map_guest_ram_dma(&container, &vm)?;

        // Scan config space for MSI-X capability
        let (msix_cap_offset, msix_table_bar_index, msix_table_offset, num_msix_vectors) =
            Self::scan_msix_capability(&device)?;

        // Phase H steps 18-20: Allocate MSI-X GSIs, create eventfds, wire to VFIO
        let msix_vectors = Vm::create_msix_group(vm.clone(), num_msix_vectors as u16)?;

        // Wire VFIO device MSI-X interrupts to eventfds
        if num_msix_vectors > 0 {
            let eventfds: Vec<_> = msix_vectors
                .vectors
                .iter()
                .map(|v| v.event_fd.as_raw_fd())
                .collect();

            device.set_irqs_eventfds(VFIO_PCI_MSIX_IRQ_INDEX, 0, &eventfds)?;
        }

        // For devices with MSI (not MSI-X), allocate a single vector and register
        // the eventfd with VFIO now. KVM routing is deferred until the guest enables
        // MSI (we read addr/data from config space at that point).
        let (msi_cap_offset, msi_is_64bit, msi_vector) = if num_msix_vectors == 0 {
            let (cap_off, is_64bit) = Self::scan_msi_capability(&device)?;
            let msi_vec = if cap_off.is_some() {
                let group = Vm::create_msix_group(vm.clone(), 1)?;
                let fd = group.vectors[0].event_fd.as_raw_fd();
                device.set_irqs_eventfds(VFIO_PCI_MSI_IRQ_INDEX, 0, &[fd])?;
                Some(group)
            } else {
                None
            };
            (cap_off, is_64bit, msi_vec)
        } else {
            (None, false, None)
        };

        Ok(VfioPciDevice {
            device,
            _container: container,
            _group: group,
            _kvm_vfio: kvm_vfio,
            sbdf,
            bars,
            msix_vectors,
            vm,
            msix_cap_offset,
            msix_table_bar_index,
            msix_table_offset,
            num_msix_vectors,
            msi_cap_offset,
            msi_is_64bit,
            msi_vector,
            bar_probe: [false; 6],
            rom_bar,
            rom_probe: false,
            rom_enable: false,
        })
    }

    /// Map all BAR regions into guest address space.
    ///
    /// For each mappable BAR:
    /// 1. mmap it from the VFIO device fd
    /// 2. Allocate a guest MMIO address from the resource allocator
    /// 3. Register the mapping with KVM via `KVM_SET_USER_MEMORY_REGION`
    fn map_bars(device: &VfioDevice, vm: &Arc<Vm>) -> Result<Vec<VfioBar>, VfioError> {
        let mut bars = Vec::new();
        let mut resource_allocator = vm.resource_allocator();

        for region in device.bar_regions() {
            // Read hardware BAR value to extract type bits
            let bar_offset = PCI_BAR0_OFFSET + (region.index as u64) * 4;
            let mut bar_val_bytes = [0u8; 4];
            device.read_config_at(bar_offset, &mut bar_val_bytes)?;
            let hw_bar_val = u32::from_le_bytes(bar_val_bytes);

            // Skip I/O BARs for M1 — only map MMIO BARs
            if hw_bar_val & PCI_BAR_IO != 0 {
                continue;
            }

            let type_bits = hw_bar_val & 0xF;
            let is_64bit = (hw_bar_val & PCI_BAR_MEM_TYPE_MASK) == PCI_BAR_MEM_TYPE_64BIT;

            if !region.is_mappable() {
                // Non-mmappable MMIO BARs are not yet supported in M1
                continue;
            }

            // Allocate guest MMIO address aligned to BAR size (PCI spec requirement).
            // PCI BARs must be naturally aligned: a 256 MiB BAR must be at a 256 MiB boundary.
            //
            // 32-bit non-prefetchable BARs MUST be below 4 GiB — the BAR register is only
            // 32 bits wide, so any address we report gets truncated by the guest on readback.
            // 64-bit prefetchable BARs go into the 64-bit window (no upper-address truncation).
            let align = region.size.next_power_of_two();
            let guest_addr = if is_64bit {
                resource_allocator
                    .allocate_64bit_mmio_memory(region.size, align, vm_allocator::AllocPolicy::FirstMatch)
                    .map_err(VfioError::ResourceAllocation)?
            } else {
                resource_allocator
                    .allocate_32bit_mmio_memory(region.size, align, vm_allocator::AllocPolicy::FirstMatch)
                    .map_err(VfioError::ResourceAllocation)?
            };

            // mmap the BAR from the VFIO device fd
            let host_ptr = device.mmap_bar(region)?;

            // Allocate a KVM memory slot
            let kvm_slot = vm
                .next_kvm_slot(1)
                .ok_or(VfioError::NoKvmSlot)?;

            // Register the BAR mapping with KVM
            let kvm_region = kvm_userspace_memory_region {
                slot: kvm_slot,
                flags: 0,
                guest_phys_addr: guest_addr,
                memory_size: region.size,
                userspace_addr: host_ptr as u64,
            };

            vm.set_user_memory_region(kvm_region)
                .map_err(|e| VfioError::Io(std::io::Error::other(format!("KVM_SET_USER_MEMORY_REGION: {e}"))))?;

            bars.push(VfioBar {
                region_index: region.index,
                type_bits,
                is_64bit,
                guest_addr,
                size: region.size,
                host_ptr,
                kvm_slot,
            });
        }

        Ok(bars)
    }

    /// Map the expansion ROM region (VFIO region 6) into guest address space, if present.
    ///
    /// Returns `Some(VfioBar)` if the ROM was mapped, `None` if the device has no ROM or
    /// the ROM region is not mappable (in which case reg 12 reads return 0).
    fn map_rom_bar(device: &VfioDevice, vm: &Arc<Vm>) -> Result<Option<VfioBar>, VfioError> {
        let region = match device.rom_region() {
            Some(r) if r.size > 0 && r.is_mappable() => *r,
            _ => return Ok(None),
        };

        // ROM BARs don't need to be DMA targets, but must be page-aligned for mmap.
        // Use size-aligned allocation (rounded up to next power of two) to satisfy
        // both the PCI size-mask probe protocol and guest resource allocator requirements.
        let align = region.size.next_power_of_two().max(4096);
        let guest_addr = vm
            .resource_allocator()
            .allocate_64bit_mmio_memory(region.size, align, vm_allocator::AllocPolicy::FirstMatch)
            .map_err(VfioError::ResourceAllocation)?;

        let host_ptr = device.mmap_bar(&region)?;

        let kvm_slot = vm.next_kvm_slot(1).ok_or(VfioError::NoKvmSlot)?;

        // Register the ROM mapping with KVM so the guest can read the vBIOS.
        // The ROM enable bit (bit 0 of reg 12) gates host-side access but the KVM
        // memory slot is always present; the guest driver enables the BAR before reading.
        let kvm_region = kvm_userspace_memory_region {
            slot: kvm_slot,
            flags: 0,
            guest_phys_addr: guest_addr,
            memory_size: region.size,
            userspace_addr: host_ptr as u64,
        };
        vm.set_user_memory_region(kvm_region).map_err(|e| {
            VfioError::Io(std::io::Error::other(format!(
                "KVM_SET_USER_MEMORY_REGION (ROM): {e}"
            )))
        })?;

        Ok(Some(VfioBar {
            region_index: VFIO_PCI_ROM_REGION_INDEX,
            type_bits: 0, // ROM BAR has no I/O or memory type bits
            is_64bit: false,
            guest_addr,
            size: region.size,
            host_ptr,
            kvm_slot,
        }))
    }

    /// Map all guest RAM regions into the IOMMU via `VFIO_IOMMU_MAP_DMA`.
    ///
    /// This allows the passthrough device to DMA directly to/from guest memory
    /// with IOMMU protection.
    fn map_guest_ram_dma(container: &VfioContainer, vm: &Arc<Vm>) -> Result<(), VfioError> {
        use crate::vstate::memory::{GuestMemory, GuestMemoryExtension};

        for region in vm.guest_memory().iter() {
            let host_addr = region
                .get_host_address(vm_memory::MemoryRegionAddress(0))
                .map_err(|e| VfioError::Io(std::io::Error::other(format!("guest memory addr: {e}"))))?;

            let iova = region.start_addr().0;
            let size = region.len();

            container.map_dma(host_addr as u64, iova, size)?;
        }

        Ok(())
    }

    /// Scan the PCI config space capability list for MSI-X.
    ///
    /// Returns `(cap_offset, table_bar_index, table_offset_in_bar, num_vectors)`.
    /// `cap_offset` is `None` if the device has no MSI-X capability.
    fn scan_msix_capability(
        device: &VfioDevice,
    ) -> Result<(Option<u32>, Option<u32>, u64, u32), VfioError> {
        // Check Capabilities List Present bit in Status register
        let mut status = [0u8; 2];
        device.read_config_at(PCI_STATUS_OFFSET, &mut status)?;
        let status_val = u16::from_le_bytes(status);

        if status_val & (1 << 4) == 0 {
            // No capability list
            return Ok((None, None, 0, 0));
        }

        // Read capability pointer
        let mut cap_ptr_buf = [0u8; 1];
        device.read_config_at(PCI_CAP_POINTER_OFFSET, &mut cap_ptr_buf)?;
        let mut cap_offset = cap_ptr_buf[0] as u32 & !0x3; // align to 4 bytes

        // Walk the capability list
        while cap_offset != 0 && cap_offset < 256 {
            let mut cap_header = [0u8; 2]; // [0]=ID, [1]=next_ptr
            device.read_config_at(cap_offset as u64, &mut cap_header)?;

            if cap_header[0] == PCI_CAP_ID_MSIX {
                // Found MSI-X capability. Read control and table location.
                let mut ctrl_bytes = [0u8; 2];
                device.read_config_at(
                    cap_offset as u64 + MSIX_CAP_CONTROL_OFFSET as u64,
                    &mut ctrl_bytes,
                )?;
                let ctrl = u16::from_le_bytes(ctrl_bytes);
                // Table size is encoded as (N-1) in bits [10:0]
                let num_vectors = (ctrl & 0x7FF) as u32 + 1;

                let mut table_bir_bytes = [0u8; 4];
                device.read_config_at(
                    cap_offset as u64 + MSIX_CAP_TABLE_OFFSET as u64,
                    &mut table_bir_bytes,
                )?;
                let table_bir = u32::from_le_bytes(table_bir_bytes);
                let table_bar_index = table_bir & 0x7;
                let table_offset = (table_bir & !0x7) as u64;

                return Ok((
                    Some(cap_offset),
                    Some(table_bar_index),
                    table_offset,
                    num_vectors,
                ));
            }

            // Follow next pointer
            cap_offset = cap_header[1] as u32 & !0x3;
        }

        // No MSI-X capability found
        Ok((None, None, 0, 0))
    }

    /// Scan the PCI config space capability list for the MSI capability (ID 0x05).
    ///
    /// Returns `(cap_offset, is_64bit)`.  `cap_offset` is `None` if no MSI capability exists.
    fn scan_msi_capability(device: &VfioDevice) -> Result<(Option<u32>, bool), VfioError> {
        let mut status = [0u8; 2];
        device.read_config_at(PCI_STATUS_OFFSET, &mut status)?;
        if u16::from_le_bytes(status) & (1 << 4) == 0 {
            return Ok((None, false));
        }

        let mut cap_ptr_buf = [0u8; 1];
        device.read_config_at(PCI_CAP_POINTER_OFFSET, &mut cap_ptr_buf)?;
        let mut cap_offset = cap_ptr_buf[0] as u32 & !0x3;

        while cap_offset != 0 && cap_offset < 256 {
            let mut cap_header = [0u8; 2];
            device.read_config_at(cap_offset as u64, &mut cap_header)?;

            if cap_header[0] == PCI_CAP_ID_MSI {
                let mut ctrl_bytes = [0u8; 2];
                device.read_config_at(
                    cap_offset as u64 + MSI_CAP_CONTROL_OFFSET as u64,
                    &mut ctrl_bytes,
                )?;
                let ctrl = u16::from_le_bytes(ctrl_bytes);
                let is_64bit = ctrl & MSI_CTRL_64BIT != 0;
                return Ok((Some(cap_offset), is_64bit));
            }

            cap_offset = cap_header[1] as u32 & !0x3;
        }

        Ok((None, false))
    }

    /// Handle MSI enable: read addr/data from config space and set up KVM routing + irqfd.
    ///
    /// Called from `write_config_register` when the guest sets the MSI enable bit.
    /// At this point the driver has already written the destination address and data
    /// to the MSI capability registers, so we read them back and programme KVM.
    fn enable_msi(&self) -> Result<(), VfioError> {
        let (Some(cap_off), Some(group)) = (self.msi_cap_offset, self.msi_vector.as_ref()) else {
            return Ok(());
        };

        let mut addr_lo_bytes = [0u8; 4];
        self.device.read_config_at(
            cap_off as u64 + MSI_CAP_ADDR_LO_OFFSET,
            &mut addr_lo_bytes,
        )?;
        let addr_lo = u32::from_le_bytes(addr_lo_bytes);

        let (addr_hi, data_offset) = if self.msi_is_64bit {
            let mut addr_hi_bytes = [0u8; 4];
            self.device.read_config_at(
                cap_off as u64 + MSI_CAP_ADDR_HI_OFFSET,
                &mut addr_hi_bytes,
            )?;
            (u32::from_le_bytes(addr_hi_bytes), MSI_CAP_DATA_64_OFFSET)
        } else {
            (0u32, MSI_CAP_DATA_32_OFFSET)
        };

        let mut data_bytes = [0u8; 4];
        self.device
            .read_config_at(cap_off as u64 + data_offset, &mut data_bytes)?;
        let data = u32::from_le_bytes(data_bytes);

        let msi_config = MsixVectorConfig {
            low_addr: addr_lo,
            high_addr: addr_hi,
            data,
            devid: self.sbdf,
        };

        let vector = &group.vectors[0];
        self.vm
            .register_msi(vector, false, msi_config)
            .map_err(|e| VfioError::Io(std::io::Error::other(format!("register_msi: {e}"))))?;
        self.vm.set_gsi_routes().map_err(VfioError::Interrupt)?;
        vector.enable(&self.vm.fd()).map_err(VfioError::Interrupt)?;

        Ok(())
    }

    /// Handle MSI-X enable: read the hardware table and set up KVM routing + irqfds.
    ///
    /// Called from `write_config_register` when the guest enables MSI-X.
    /// Runs on the vcpu thread (KVM_IRQFD and KVM_SET_GSI_ROUTING are in the vcpu filter).
    fn enable_msix(&self) -> Result<(), VfioError> {
        let Some(table_bar_index) = self.msix_table_bar_index else {
            return Ok(());
        };

        // Find the mmap'd host address for the BAR containing the MSI-X table
        let table_bar = self.bars.iter().find(|b| b.region_index == table_bar_index);
        let Some(table_bar) = table_bar else {
            return Ok(());
        };

        if table_bar.host_ptr.is_null() {
            return Ok(());
        }

        // Read the MSI-X table from the mmap'd BAR
        for (i, vector) in self.msix_vectors.vectors.iter().enumerate() {
            let entry_offset = self.msix_table_offset as usize + i * MSIX_ENTRY_SIZE;

            // SAFETY: host_ptr is a valid mmap'd BAR address; entry_offset + MSIX_ENTRY_SIZE
            // lies within the BAR (we validated the MSI-X table is in this BAR at construction).
            // Reads are volatile to avoid the compiler caching stale values; the guest may have
            // written these fields via the KVM memory slot mapping.
            let addr_lo = unsafe {
                std::ptr::read_volatile(table_bar.host_ptr.add(entry_offset) as *const u32)
            };
            let addr_hi = unsafe {
                std::ptr::read_volatile(table_bar.host_ptr.add(entry_offset + 4) as *const u32)
            };
            let data = unsafe {
                std::ptr::read_volatile(table_bar.host_ptr.add(entry_offset + 8) as *const u32)
            };
            let vector_ctrl = unsafe {
                std::ptr::read_volatile(table_bar.host_ptr.add(entry_offset + 12) as *const u32)
            };

            let masked = vector_ctrl & MSIX_ENTRY_VECTOR_CTRL_MASKED != 0;

            let msi_config = MsixVectorConfig {
                low_addr: addr_lo,
                high_addr: addr_hi,
                data,
                devid: self.sbdf,
            };

            self.vm
                .register_msi(vector, masked, msi_config)
                .map_err(|e| VfioError::Io(std::io::Error::other(format!("register_msi: {e}"))))?;
        }

        // Push the updated routing table to KVM
        self.vm.set_gsi_routes().map_err(VfioError::Interrupt)?;

        // Register irqfds for unmasked vectors
        for vector in &self.msix_vectors.vectors {
            vector
                .enable(&self.vm.fd())
                .map_err(VfioError::Interrupt)?;
        }

        Ok(())
    }

    /// Return the value to report for BAR register `reg_idx` (4–9).
    ///
    /// Substitutes the guest-allocated address while preserving hardware type bits.
    /// Returns `None` if no mapped BAR corresponds to this register index.
    fn bar_register_value(&self, reg_idx: u16) -> Option<u32> {
        let bar_index = (reg_idx - PCI_BAR0_REG_IDX) as u32;

        // Check for a direct BAR match
        if let Some(bar) = self.bars.iter().find(|b| b.region_index == bar_index) {
            // Lower half of a 64-bit BAR, or a 32-bit BAR
            let addr_low = bar.guest_addr as u32;
            return Some((addr_low & !0xF) | bar.type_bits);
        }

        // Check if this reg_idx is the upper 32 bits of a 64-bit BAR
        if bar_index > 0 {
            let prev_bar_index = bar_index - 1;
            if let Some(bar) = self.bars.iter().find(|b| b.region_index == prev_bar_index) {
                if bar.is_64bit {
                    return Some((bar.guest_addr >> 32) as u32);
                }
            }
        }

        None
    }

    /// Return the BAR size mask to report when the kernel probes BAR `reg_idx`.
    ///
    /// The PCI BAR probe protocol: the kernel writes 0xFFFFFFFF to a BAR, reads back
    /// `~(size-1) | type_bits`, then writes the original value to restore it.
    /// Without returning a correct size mask the kernel assigns no resource to the device.
    ///
    /// Returns `None` if no BAR (or upper 64-bit slot) corresponds to this register.
    fn bar_size_mask(&self, reg_idx: u16) -> Option<u32> {
        let bar_index = (reg_idx - PCI_BAR0_REG_IDX) as u32;

        // Direct match: this reg_idx is the (lower) register for a mapped BAR.
        if let Some(bar) = self.bars.iter().find(|b| b.region_index == bar_index) {
            // ~(size-1) gives the address mask; clear the lower nibble and put back type bits.
            let mask = (!(bar.size.wrapping_sub(1)) as u32 & !0xF_u32) | bar.type_bits;
            return Some(mask);
        }

        // Upper-32-bit slot of a 64-bit BAR.
        if bar_index > 0 {
            let prev = bar_index - 1;
            if let Some(bar) = self.bars.iter().find(|b| b.region_index == prev) {
                if bar.is_64bit {
                    // Upper 32 bits of ~(size-1).  For any BAR < 4 GiB this is 0xFFFF_FFFF.
                    return Some((!(bar.size.wrapping_sub(1)) >> 32) as u32);
                }
            }
        }

        None
    }
}

impl PciDevice for VfioPciDevice {
    fn read_config_register(&mut self, reg_idx: u16) -> u32 {
        // Intercept BAR registers
        if (PCI_BAR0_REG_IDX..=PCI_BAR5_REG_IDX).contains(&reg_idx) {
            let bar_slot = (reg_idx - PCI_BAR0_REG_IDX) as usize;
            // If the kernel wrote 0xFFFF_FFFF to this slot, return the size mask.
            // For unhandled BARs (e.g. I/O BARs we skip) return 0 so the guest sees
            // "no resource here" rather than a bogus size from the host hardware read
            // (which triggers "[Firmware Bug]: BAR N: invalid; can't size").
            if bar_slot < 6 && self.bar_probe[bar_slot] {
                return self.bar_size_mask(reg_idx).unwrap_or(0);
            }
            if let Some(val) = self.bar_register_value(reg_idx) {
                return val;
            }
            // Unhandled BAR slot (I/O BAR or non-mappable region). Return 0 rather than
            // proxying the host BAR value — a host physical/IO address is meaningless
            // in the guest and could leak host layout.
            return 0;
        }

        // Intercept expansion ROM BAR register (offset 0x30).
        //
        // The PCI probe protocol for the ROM BAR:
        //   1. Guest writes 0xFFFFFFFF → we set rom_probe = true
        //   2. Guest reads  → we return ~(size-1) with bit 0 cleared (per PCI spec)
        //   3. Guest writes restore value → we clear rom_probe, update rom_enable
        //   4. Guest reads  → we return guest_addr | rom_enable
        //
        // If no ROM was mapped we always return 0 to prevent leaking host addresses.
        if reg_idx == PCI_ROM_REG_IDX {
            if let Some(rom) = &self.rom_bar {
                if self.rom_probe {
                    // Size mask: ~(size-1) with bit 0 cleared (ROM enable bit; hardware clears
                    // it during probe to prevent accidental ROM access while sizing).
                    return !(rom.size.wrapping_sub(1)) as u32 & !1u32;
                }
                return rom.guest_addr as u32 | (self.rom_enable as u32);
            }
            // No ROM (absent or not mappable): return 0 so the guest ignores this BAR.
            return 0;
        }

        // Proxy all other registers through the VFIO device fd
        let byte_offset = reg_idx as u64 * 4;
        let mut buf = [0u8; 4];
        if let Err(e) = self.device.read_config_at(byte_offset, &mut buf) {
            crate::logger::error!(
                "VfioPciDevice: config read at reg_idx {reg_idx} failed: {e}"
            );
        }
        u32::from_le_bytes(buf)
    }

    fn write_config_register(
        &mut self,
        reg_idx: u16,
        offset: u8,
        data: &[u8],
    ) -> Option<Arc<Barrier>> {
        // Intercept BAR writes: track probe state, never forward to hardware.
        if (PCI_BAR0_REG_IDX..=PCI_BAR5_REG_IDX).contains(&reg_idx) {
            let bar_slot = (reg_idx - PCI_BAR0_REG_IDX) as usize;
            if bar_slot < 6 {
                // A BAR size probe is a full 32-bit all-ones write at offset 0.
                // The restore write (any other value) clears the probe flag so that
                // subsequent reads return the guest address again.
                if offset == 0 && data.len() == 4 {
                    let val = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    self.bar_probe[bar_slot] = val == 0xFFFF_FFFF;
                } else {
                    self.bar_probe[bar_slot] = false;
                }
            }
            return None;
        }

        // Intercept ROM BAR writes (reg 12 / offset 0x30).
        // Never forward to hardware — the VFIO mmap handles physical access.
        if reg_idx == PCI_ROM_REG_IDX {
            if offset == 0 && data.len() >= 4 {
                let val = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                // Probe detection: address bits [31:11] all-ones (guest wrote 0xFFFFFFFF
                // or 0xFFFFFFFE to size the BAR).
                self.rom_probe = (val & 0xFFFF_F800) == 0xFFFF_F800;
                // Track enable bit for the guest address read-back.
                self.rom_enable = val & 1 != 0;
            }
            return None;
        }

        // Check for MSI enable: the guest sets bit 0 of the MSI message control register.
        // The driver will have written the destination address and data to the capability
        // registers just before this, so we can read them back to programme KVM routing.
        if let Some(cap_off) = self.msi_cap_offset {
            let write_byte_offset = reg_idx as u32 * 4 + offset as u32;
            let msi_ctrl_byte = cap_off + MSI_CAP_CONTROL_OFFSET as u32;

            if write_byte_offset <= msi_ctrl_byte
                && write_byte_offset + data.len() as u32 > msi_ctrl_byte
            {
                let ctrl_byte_in_data = (msi_ctrl_byte - write_byte_offset) as usize;
                if ctrl_byte_in_data < data.len() {
                    // MSI enable is bit 0 of the first byte of the control word
                    if data[ctrl_byte_in_data] & 0x01 != 0 {
                        if let Err(e) = self.enable_msi() {
                            crate::logger::error!("VfioPciDevice: MSI enable failed: {e}");
                        }
                    }
                }
            }
        }

        // Check for MSI-X enable: the guest writes to the MSI-X capability control register
        if let Some(cap_off) = self.msix_cap_offset {
            let write_byte_offset = reg_idx as u32 * 4 + offset as u32;
            let msix_ctrl_byte = cap_off + MSIX_CAP_CONTROL_OFFSET as u32;

            // MSI-X control is a u16 at cap_off+2; check if this write touches it
            if write_byte_offset <= msix_ctrl_byte
                && write_byte_offset + data.len() as u32 > msix_ctrl_byte
            {
                // Extract the u16 control value from the write data
                let ctrl_byte_in_data = (msix_ctrl_byte - write_byte_offset) as usize;
                if ctrl_byte_in_data + 1 < data.len() {
                    let ctrl_lo = data[ctrl_byte_in_data];
                    let ctrl_hi = data[ctrl_byte_in_data + 1];
                    let ctrl = u16::from_le_bytes([ctrl_lo, ctrl_hi]);
                    if ctrl & MSIX_CTRL_ENABLE != 0 {
                        if let Err(e) = self.enable_msix() {
                            crate::logger::error!("VfioPciDevice: MSI-X enable failed: {e}");
                        }
                    }
                }
            }
        }

        // Proxy the write through to the device config space
        let byte_offset = reg_idx as u64 * 4 + offset as u64;
        if let Err(e) = self.device.write_config_at(byte_offset, data) {
            crate::logger::error!(
                "VfioPciDevice: config write at reg_idx {reg_idx} offset {offset} failed: {e}"
            );
        }

        None
    }
}

impl Drop for VfioPciDevice {
    fn drop(&mut self) {
        // Disable MSI-X vectors before tearing down
        let _ = self.msix_vectors.disable();

        // Unmap BAR KVM memory slots by setting memory_size = 0.
        // Iterate both regular BARs and the ROM bar through a single helper closure.
        let unmap_slot = |vm: &Arc<Vm>, bar: &VfioBar| {
            if !bar.host_ptr.is_null() {
                let kvm_region = kvm_userspace_memory_region {
                    slot: bar.kvm_slot,
                    flags: 0,
                    guest_phys_addr: bar.guest_addr,
                    memory_size: 0, // removing the slot
                    userspace_addr: 0,
                };
                let _ = vm.set_user_memory_region(kvm_region);

                // SAFETY: host_ptr was returned by libc::mmap; bar.size matches the mapping.
                unsafe {
                    libc::munmap(bar.host_ptr as *mut libc::c_void, bar.size as usize);
                }
            }
        };

        for bar in &self.bars {
            unmap_slot(&self.vm, bar);
        }
        if let Some(rom) = &self.rom_bar {
            unmap_slot(&self.vm, rom);
        }

        // Reset the device on teardown
        if self.device.supports_reset() {
            let _ = self.device.reset();
        }
    }
}
