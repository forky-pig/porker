# Validated hardware

Devices we've tested Porker against end-to-end on a documented host.

## Realtek RTL8111 Gigabit NIC

PCI ID: `10ec:8168` (varies by revision).

Cheap, has its own IOMMU group on typical desktop boards, supports FLR
cleanly. Used as the first test target.

What works:

- vfio-pci bind, container/group/device setup
- BAR mapping, IOMMU DMA mapping of guest RAM
- Guest `r8169` driver binds, `eth0` comes up, traffic flows


## NVIDIA RTX 4080 Max-Q (AD104M)

PCI ID: `10de:27e0`. Mobile/laptop variant. Lenovo system tested.

What works:

- All BARs map correctly (BAR0 16 MiB, BAR1 16 GiB, BAR3 32 MiB)
- nvidia OpenRM 580.126.18 binds, GSP firmware (`gsp_ga10x.bin`) loads
- `nvidia-smi -q` returns full report
- CUDA: vecadd, saxpy, reduction, matmul, multistream, bigmem, sustained
- 4-stream concurrency, per-stream correctness
- 8 GiB IOMMU-mapped DMA round-trip (28.5s wall-clock, 20 GiB guest)
- Sustained ~111K kernel launches/sec for 30s
- Teardown under load: 15/15 SIGTERM-during-sustained-launches recovers
  cleanly via vfio-pci FLR
- 3/3 stability runs back-to-back

What doesn't:

- MSI-X capability is orphaned outside the standard config-space cap chain.
  Not a Porker bug, same chain visible in host sysfs. Driver falls back to
  polling and works correctly.
- ResizableBAR has not been tested. Guest cmdline carries `pci=realloc=off`.
- Snapshot, balloon, hot-plug all out of scope (rejected at API).

IOMMU group: 16 (on the test board). Host BDF `0000:01:00.0`, guest BDF
`0000:00:01.0`.


## Untested

Anything not in the list above. The RTL8111 path is generic enough that other
PCI NICs in their own IOMMU group should work, but no claim. Other NVIDIA GPUs
likely work for compute but the MSI-X cap-chain behavior is GPU-specific.
AMD GPUs untested. aarch64 untested.

If you get something else working, drop a note in an issue with your config
and we'll add it here.
