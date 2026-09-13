# Porker

A practical, minimal, high-performance VMM with first-class device passthrough.
Forked from [Firecracker](https://github.com/firecracker-microvm/firecracker).

Porker is what Firecracker is, plus VFIO PCI passthrough that works for real
GPUs.

## What is Porker?

Porker is a Rust-based virtual machine monitor that uses Linux KVM to run
microVMs. It is a hard fork of Amazon Web Services' Firecracker, which targets
multi-tenant serverless workloads with a deliberately minimal device model.
Porker keeps Firecracker's small surface area and fast startup, but adds the
device-passthrough machinery needed to run GPU and other PCIe-attached
workloads end-to-end.

If you want maximum density, snapshotting, balloon-style memory overcommit, and
the upstream support story — use Firecracker. If you want a small VMM that can
pass a GPU through to a guest, run real CUDA workloads on it, and tear down
cleanly under load — that's the Porker bet.

Porker inherits Firecracker's Apache 2.0 license. The vast majority of this
codebase is upstream Firecracker; the VFIO passthrough additions are the
contribution. See [CREDITS.md](CREDITS.md) and [CHANGELOG.md](CHANGELOG.md).

## What works today

Validated on real hardware (see [docs/HARDWARE.md](docs/HARDWARE.md)):

- **PCIe device passthrough** via VFIO type1 — Realtek RTL8111 NIC and NVIDIA
  RTX 4080 Max-Q (AD104M)
- **NVIDIA GPU compute end-to-end** — driver bind, `nvidia-smi`, CUDA kernels,
  4-stream concurrency, ~111K kernel launches/sec sustained, 8 GiB
  IOMMU-mapped DMA round-trip
- **Clean teardown under load** — SIGTERM during sustained kernel launches
  triggers vfio-pci FLR and a fresh VM boot re-acquires the device cleanly
  (15/15 cycles validated)
- **All of Firecracker's existing virtio support** — block, net, vsock,
  entropy, pmem, balloon — when no VFIO device is attached

## What does NOT work

Honest about the holes. These are explicit non-goals for v0.1.0:

- **Snapshot + VFIO** — incompatible. Porker rejects at the API level. *(GPU
  device state isn't snapshottable through any VFIO API.)*
- **Balloon + VFIO** — incompatible. Porker rejects at the API level.
  *(Balloon-reclaimed pages remain IOMMU-mapped → DMA use-after-free risk.)*
- **PCIe hot-plug** — cold-plug only. Devices are assigned at VM creation.
- **Multi-GPU per VM** — single VFIO device per VM.
- **ResizableBAR negotiation** — disabled (`pci=realloc=off` on the guest
  cmdline). The device's default BAR sizes are used.
- **MSI-X on RTX 4080 Max-Q** — this specific GPU has its MSI-X capability
  orphaned outside the standard config-space cap chain. The nvidia driver
  falls back to polling and works correctly. Behavior on other GPUs is
  untested.
- **aarch64 VFIO** — untested. The upstream Firecracker aarch64 paths still
  exist in tree but the passthrough additions are x86_64-only for now.

See [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) for the security guarantees
that change versus upstream Firecracker — most importantly, that DMA isolation
now depends on a correctly configured host IOMMU.

## Quickstart

End-to-end walkthrough: host setup → bind device to vfio-pci → boot a
passthrough VM. See [docs/QUICKSTART.md](docs/QUICKSTART.md).

## Building from source

Porker uses Firecracker's existing build system unchanged. You need Docker
running on the host (the build runs in a development container).

```bash
git clone <fork-url>.git porker
cd porker
tools/devtool build
toolchain="$(uname -m)-unknown-linux-musl"
```

The `firecracker` and `jailer` binaries are placed at
`build/cargo_target/${toolchain}/debug/`. The binaries are still named
`firecracker` and `jailer` — only the project and release artifacts are
rebranded. See [docs/UPSTREAM.md](docs/UPSTREAM.md) for why.

## Releases

Latest release: see [GitHub releases](../../releases).
Changes are recorded in [CHANGELOG.md](CHANGELOG.md); Porker entries are at
the top, upstream Firecracker entries are preserved below.

## Project status

**Alpha.** The platform works on the validated hardware path; expect rough
edges anywhere else. v0.1.0 is the first release intended for consumption by
anyone-not-the-author.

We track upstream Firecracker for security fixes; we do **not** pursue
upstream merge for the VFIO/passthrough work — those goals diverge enough
from Firecracker's design center that upstreaming isn't realistic. See
[docs/UPSTREAM.md](docs/UPSTREAM.md) for the sync cadence and the file-level
drift surface where upstream changes will require manual merging.

## Contributing

Open a GitHub issue. Keep PRs small and focused. Most of the developer
workflow in [CONTRIBUTING.md](CONTRIBUTING.md) (inherited from Firecracker)
still applies.

## Security disclosure

Security-sensitive issues: do **not** open a public issue. The current
[SECURITY.md](SECURITY.md) is inherited from upstream Firecracker and points
to AWS contacts; that policy will be updated in a subsequent release to
point at the Porker maintainer. Until then, if you find something sensitive,
contact the maintainer directly via GitHub.

## Design

Porker's overall architecture is described in
[the design document](docs/design.md), inherited from Firecracker and still
accurate for everything except the VFIO subsystem. The VFIO additions live
under `src/vmm/src/vfio/` and `src/vmm/src/devices/pci/vfio_pci.rs`.
