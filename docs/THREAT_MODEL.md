# Threat model

What the Porker security posture is, and how it differs from upstream
Firecracker. Read this before deciding whether Porker is right for your
workload.

## What Porker keeps from Firecracker

Same KVM-based vCPU isolation, same seccomp filter architecture, same jailer
chroot model. If you don't attach a VFIO device, the attack surface is
roughly Firecracker's. Roughly, because we did add VFIO ioctls to the
seccomp filter even when no VFIO device is attached (the filter is the same
binary).

## What changes when a VFIO device is attached

This is the core of it. With VFIO passthrough, the security model picks up
new dependencies that weren't there in upstream FC.

### DMA isolation depends on the host IOMMU

In upstream FC, guest memory isolation is provided by KVM (EPT) plus seccomp
plus jailer. The guest can't reach host memory it doesn't own.

With VFIO, the guest has a real PCIe device that does real DMA. DMA bypasses
KVM entirely. The only thing keeping the guest's device from DMA-ing
anywhere in host memory is the IOMMU.

This means:

- IOMMU must be enabled (`intel_iommu=on` or `amd_iommu=on` on the host
  cmdline).
- IOMMU groups must be sane. If the GPU shares a group with a host NIC,
  passing the GPU through means the NIC is in the group too. Porker checks
  this and rejects.
- IOMMU bypass modes (passthrough, identity mappings) must not be active.
- A buggy or misconfigured IOMMU = guest can DMA anywhere = full host
  compromise. There is no software backstop.

### VFIO ioctl surface in seccomp

The vmm and vcpu thread filters include VFIO ioctls (group, container,
device, IOMMU map/unmap). Each of these reaches into kernel code paths that
upstream FC didn't reach. The kernel VFIO code is widely reviewed but not
zero-CVE history.

### GPU firmware is in the trust boundary

A GPU has its own firmware (NVIDIA: GSP-RM, AMD: SMC/PSP). With passthrough,
guest workloads talk directly to that firmware. A compromised guest could
attack GPU firmware bugs to affect:

- Other VMs that later attach the same GPU (via persistent state in
  unreset device memory or firmware-resident state)
- The host, if firmware bugs allow privilege escalation through the PCIe
  fabric

We don't have a story for verifying GPU firmware integrity. FLR resets a
lot but doesn't reset everything.

### PCIe ACS is required for safe peer-to-peer

PCI Access Control Services (ACS) prevents peer-to-peer DMA between PCIe
endpoints. Without ACS, a compromised passthrough device can DMA to other
devices on the same root complex without going through the IOMMU.

ACS is typically enabled on server platforms with proper PCIe switches.
On consumer/desktop boards, support varies. If your board doesn't have ACS
on the relevant root port, peer-to-peer DMA is reachable.

Porker doesn't currently verify ACS state. Check it yourself with
`lspci -vvv` and look for the AccessControl Services capability with
SrcValid+ TransBlk+.

### Snapshot is rejected, not "supported with caveats"

Upstream FC has snapshot/restore. With a VFIO device attached, snapshot is
not implementable correctly: GPU device state (VRAM contents, register
state, command queues, firmware state) is not exposed through any VFIO API.

Porker rejects snapshot-with-VFIO at the API. If you bypass the check
somehow, the resulting state file is unrestoreable.

### Balloon is rejected, not "supported with caveats"

Same shape. The balloon device works by guest cooperation: guest tells
host "you can reclaim these pages." Host munmaps them. With VFIO, those
pages are still IOMMU-mapped. The device can DMA into them after the host
has reclaimed them. That's a use-after-free in the DMA path, exploitable
for host memory corruption.

Porker rejects balloon-with-VFIO at the API.

### Reset is best-effort

When a VM exits, the GPU should be reset before the next workload uses it.
We do FLR via vfio-pci on device close. This works on the validated GPU
(15/15 cycles tested). It does not work on all hardware. NVIDIA consumer
GPUs from Maxwell through Turing have known FLR issues.

If you deploy this multi-tenant, you take responsibility for confirming
clean reset on your specific hardware. Porker logs reset attempts but
doesn't currently fail-fast on a stuck device.

## What we don't claim

- We don't claim Porker is CC-EAL anything. Upstream FC isn't either, but
  some of FC's published threat model assumes a Lambda-like operational
  envelope. Porker doesn't.
- We don't claim Porker is hardened against side-channel attacks via the
  GPU. Speculative execution, cache, or DMA-based covert channels through
  a passthrough device are out of scope.
- We don't claim Porker is hardened against malicious kernel modules in
  the guest. With device passthrough this gets weird because the guest
  driver is touching real hardware.

## What we do claim

- The validated hardware path runs the workloads we've tested. We've
  not seen a host crash, kernel splat, IOMMU fault, or DMA error.
- The footgun configurations (snapshot+VFIO, balloon+VFIO) are blocked at
  the API and produce a clear error.
- Cross-tenant: with a cooperating host setup (IOMMU, ACS, FLR-capable
  GPU, no shared IOMMU group members), back-to-back VMs on the same GPU
  do not see each other's state.

## Practical advice

- Single-tenant, single-GPU: Porker is probably fine.
- Multi-tenant, same physical host, dedicated GPU per VM: do your own
  hardware validation for clean reset.
- Multi-tenant, GPU shared across tenants without VM-level isolation:
  not supported by anything Porker does.
- Snapshotting workloads: don't use Porker. Use upstream FC.
- Workloads that need balloon-style memory overcommit: don't use Porker.
  Use upstream FC.
