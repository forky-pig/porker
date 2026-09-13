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

`src/vmm/src/vstate/interrupts.rs` and `src/vmm/src/vstate/vm.rs`. The MSI-X
vector API and the KVM VM handle. `VfioPciDevice` programs MSI/MSI-X routes
through these; upstream reshaped both in 1.16/1.17 (`MsixVectorConfig` ->
`MsixTableEntry`, `Vm` -> `KvmVm`). Semantic, not just textual: check how
masked vectors are treated (see the MSI-X note in the changelog).

`src/vmm/src/device_manager/mod.rs`. How the device manager exposes PCI
state (`VirtioDevices` enum since 1.17). The VFIO attach loop in the builder
and the snapshot check in `lib.rs` reach `PciDevices` through it.

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

What worked for the v1.17.0 sync (0.1.1); repeat it.

1. `git fetch origin` (upstream). Pick checkpoints: the fork points of
   upstream's release tags on `main` (the tag itself if it is one commit
   off `main`). Branch `sync/upstream-<tag>` off the Porker branch
   (`porker-internal` in the development fork; `main` in porker-vmm/porker
   only ever receives snapshots, see "Publishing" below).
2. Per checkpoint: `git merge --no-ff <commit>`. Stripped CI stays deleted,
   including *new* files upstream adds under `.buildkite/`. `README.md` is
   Porker's. `CHANGELOG.md` keeps the Porker sections on top. Walk every
   drift-surface conflict by hand; expect compile-level adaptation in
   `vfio_pci.rs` even where there is no textual conflict.
3. Build: `cargo build --release -p firecracker -p jailer`. The workspace's
   `default-members` excludes the jailer, so a plain `cargo build` leaves a
   stale jailer binary. `cargo clippy` too (install the clippy component
   for the new toolchain first).
4. Unit tests, twice: `RUST_TEST_THREADS=1 cargo test -p vmm -p jailer
   --no-fail-fast` on the merge and on pure upstream at the same commit.
   The two failure sets must be identical. Upstream runs single-threaded;
   dozens of tests assert on global metrics and flake in parallel. TAP tests
   need root. Some failures are host-specific and permanent (pmem/virtio-mem
   on 39-bit CPUs), which is why the comparison is against upstream and not
   against zero.
5. Boot smoke: a plain guest to login over virtio-mmio and again with
   `--enable-pci`, then a clean SIGTERM exit.
6. Hardware: `sudo tools/porker_hw_round.sh all` (GPU: M3 stability, 3
   boots + teardown; NIC: M1, guest driver binds the card). Its header lists
   the traps it works around; do not reinvent them.
7. Commit the merge with the resolution in the message. Bump
   `PORKER_FORK_BASE` (firecracker and jailer `main.rs`). Fast-forward the
   Porker branch, push.
8. Update this file's "Currently tracked upstream base" and the changelog,
   then publish.

## Publishing

porker-vmm/porker is publish-only: a single squashed snapshot per release,
never edited directly (edits made there have to be back-ported here first).
`tools/publish_snapshot.sh` copies the tagged internal tree over the public
checkout minus the strip list (`tools/publish_strip.txt`), leaving one
commit and one tag for review and push. Merge commits cannot be mirrored
with `tools/mirror_to_porker.sh`; that script is for single commits only.

## What Porker doesn't take from upstream

- Buildkite pipelines, AWS-internal automation, FC's Slack notifications.
  Removed at fork time. If upstream restructures these, ignore.
- Test artifacts that depend on FC's CI infrastructure.
- AWS-specific documentation in `docs/` that references FC's testing on
  AWS instance types. We don't run on those, the docs are stale-but-
  harmless, will be cleaned up incrementally.

## Currently tracked upstream base

Fork base SHA: `95f868c8e` = Firecracker **v1.17.0** (2026-09-04). This is
what `firecracker --version` reports.

Last upstream sync: 2026-09-13, Porker 0.1.1. Two merge commits:
`88955789c` (upstream main at the v1.16.0 cut, `e90cef8b6`) and `4440262d9`
(the v1.17.0 tag). The original fork point was `a481d51d0` (2026-04-14),
which 0.1.0 shipped on.
