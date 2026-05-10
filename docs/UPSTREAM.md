# Upstream tracking

Porker is a hard fork of Firecracker. We pull upstream's security fixes; we
don't pursue upstream merge for the VFIO/passthrough work.

## Why a hard fork

Firecracker's design center is multi-tenant serverless density: minimal
device model, snapshot/restore, balloon-based overcommit. VFIO passthrough
adds requirements (real PCIe topology, IOMMU dependency, no overcommit
safety, no snapshot) that pull against that center. The work was done as a
fork from the start; trying to upstream it would force compromises that
defeat the point.

If upstream's posture changes (issues `#5687` and `#5679` are roadmap-
tracked), we can revisit. Until then: hard fork.

## Sync cadence

- **Security advisories from upstream FC**: review immediately. Cherry-pick
  and apply. Skip the rest of upstream's stack unless there's a reason to
  pull more.
- **Upstream `main` review**: monthly. Read the changelog, scan for things
  that touch our drift surface (next section). No automatic merge.
- **Upstream tagged releases**: opt-in. We don't track upstream version
  numbers. Porker has its own semver.

## Drift surface

Files where Porker's additions overlap with files upstream FC continues
to evolve. These are where merge conflicts will happen during sync. Risk
graded by how often upstream touches the file and how invasively our
changes overlay.

### High risk

`src/vmm/src/device_manager/pci_mngr.rs`. Upstream is actively iterating on
PCI device management. Our additions register VFIO devices alongside virtio
in the same data structures. A non-trivial upstream refactor here will
need careful merge.

`src/vmm/src/builder.rs`. Upstream edits the build path frequently. We
added VFIO setup pre-seccomp. Conflicts likely on most non-trivial syncs.

### Medium risk

`src/vmm/src/rpc_interface.rs`. We add VFIO API endpoints and the
balloon/snapshot rejection logic. Upstream edits this for new device APIs.

`src/jailer/src/env.rs` and `src/jailer/src/main.rs`. We added VFIO
fd-passing. Jailer evolves slowly upstream but fundamentals (chroot,
capability drop) are touched occasionally.

`resources/seccomp/x86_64-unknown-linux-musl.json`. We added VFIO ioctls.
Upstream changes filter when they add features. Conflicts here are
JSON-merge-shaped, not semantic, but check carefully.

`src/vmm/src/pci/mod.rs`. The `PciDevice` trait shape. Our `VfioPciDevice`
implements it. Upstream changing the trait means we adapt.

### Low risk

`src/vmm/src/vfio/` (entire module). New, Porker-only. Won't conflict.

`src/vmm/src/devices/pci/vfio_pci.rs`. New, Porker-only. Depends on the
trait shape (medium-risk above) but the file itself doesn't conflict.

## Merge process

When pulling an upstream change:

1. Cherry-pick or merge into a `sync/upstream-<sha>` branch off `main`.
2. Run `cargo build` and exercise the VFIO path on real hardware (a NIC
   VF is the cheapest signal; a GPU is the heavier one).
3. If the change touches a high-risk file, walk the conflict by hand;
   don't trust automatic merge.
4. If the change touches the seccomp filter, regenerate and confirm the
   VFIO smoke test still passes through jailer.
5. PR into `main`. Update this file's drift surface section if a new
   conflict zone shows up.

## What Porker doesn't take from upstream

- Buildkite pipelines, AWS-internal automation, FC's Slack notifications.
  Removed at fork time. If upstream restructures these, ignore.
- Test artifacts that depend on FC's CI infrastructure.
- AWS-specific documentation in `docs/` that references FC's testing on
  AWS instance types. We don't run on those, the docs are stale-but-
  harmless, will be cleaned up incrementally.

## Currently tracked upstream base

Fork base SHA: `a481d51d0` (the upstream FC commit Porker forked from).
This is what `firecracker --version` reports.

Last upstream sync: at fork time. None since. v0.1.0 is the initial
release; subsequent releases will record the last-synced upstream SHA
here.
