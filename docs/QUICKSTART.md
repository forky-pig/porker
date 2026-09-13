# Quickstart

Boot a Porker VM with a passthrough PCI device. Validated path: x86_64 Linux
host, RTL8111 NIC or NVIDIA GPU with FLR support, single VFIO device per VM.

If your hardware isn't in `docs/HARDWARE.md`, this might still work, but you
may need to adapt.

## Host prerequisites

Linux host kernel 5.15 or later. Tested with 6.17.

IOMMU enabled in BIOS/UEFI and on the kernel cmdline:

```
intel_iommu=on    # Intel CPUs
amd_iommu=on      # AMD CPUs
iommu=pt          # optional, passthrough mode for non-VFIO devices
```

vfio-pci modules loaded:

```bash
modprobe vfio vfio_iommu_type1 vfio-pci
```

KVM accessible (`/dev/kvm`).

## Bind your device to vfio-pci

Find the device. Example for the validated NIC:

```bash
lspci -nn | grep RTL8111
# 03:00.0 Ethernet controller [0200]: Realtek RTL8111 [10ec:8168] (rev 15)
```

Find its IOMMU group:

```bash
readlink /sys/bus/pci/devices/0000:03:00.0/iommu_group
# ../../../../kernel/iommu_groups/14
```

Group 14 in this case. Check what else is in the group:

```bash
ls /sys/kernel/iommu_groups/14/devices/
```

If anything other than your target device shows up, those have to be passed
through too, or you need a different host with better IOMMU isolation.
Porker rejects attach if the group is contaminated.

Unbind the current driver and bind to vfio-pci:

```bash
DEV=0000:03:00.0
echo "$DEV" > /sys/bus/pci/devices/$DEV/driver/unbind
echo vfio-pci > /sys/bus/pci/devices/$DEV/driver_override
echo "$DEV" > /sys/bus/pci/drivers/vfio-pci/bind
```

Verify:

```bash
lspci -k -s $DEV | grep "Kernel driver"
# Kernel driver in use: vfio-pci
ls /dev/vfio/
# 14  vfio
```

`/dev/vfio/14` is your group fd source. `/dev/vfio/vfio` is the container.

## Minimal config

You need a guest kernel and rootfs. For the guest cmdline, include
`pci=realloc=off` (ResizableBAR is disabled in v0.1.x).

The Porker API is HTTP-over-Unix-socket. Sequence:

1. Start `firecracker` with the API socket and the VFIO fds inherited.
2. PUT `/machine-config` (vcpu count, memory).
3. PUT `/boot-source` (kernel, cmdline).
4. PUT `/drives/rootfs` (rootfs image).
5. PUT `/vfio-device` (host BDF, container fd, group fd, IOMMU group id).
6. PUT `/actions {"action_type": "InstanceStart"}`.

VFIO fds need to be inherited across the firecracker exec. Two patterns
work today:

- **From a parent process** that opens the fds and `exec`s firecracker
  with them inherited (Python: `subprocess.Popen(pass_fds=...)`). The
  fd numbers are then passed to the API in the `PUT /vfio-device` body.
- **Through `jailer`** with the new `--vfio-container-fd` and
  `--vfio-group-fd` arguments. The caller opens the files, hands the fd
  numbers to jailer, jailer preserves them across chroot+exec.

## Minimal example

A small wrapper, in any language, that opens the VFIO fds and drives
the API. In Python:

```python
import os, socket, subprocess, json, time

GROUP_ID = 14            # IOMMU group of your device
HOST_BDF = "0000:03:00.0"
SOCK     = "/tmp/fc.sock"

container_fd = os.open("/dev/vfio/vfio", os.O_RDWR)
group_fd     = os.open(f"/dev/vfio/{GROUP_ID}", os.O_RDWR)

try: os.unlink(SOCK)
except FileNotFoundError: pass

fc = subprocess.Popen(
    ["./firecracker", "--api-sock", SOCK, "--enable-pci"],
    pass_fds=(container_fd, group_fd),
)

# Wait for the API socket to appear, then send the configuration.
while not os.path.exists(SOCK): time.sleep(0.05)

def api(method, path, body):
    req = (
        f"{method} {path} HTTP/1.0\r\n"
        f"Content-Type: application/json\r\n"
        f"Content-Length: {len(body)}\r\n\r\n{body}"
    )
    s = socket.socket(socket.AF_UNIX); s.connect(SOCK)
    s.sendall(req.encode()); resp = s.recv(4096); s.close()
    return resp

api("PUT", "/machine-config", json.dumps(
    {"vcpu_count": 2, "mem_size_mib": 1024}))
api("PUT", "/boot-source", json.dumps(
    {"kernel_image_path": "/path/to/vmlinux",
     "boot_args": "console=ttyS0 root=/dev/vda rw pci=realloc=off"}))
api("PUT", "/drives/rootfs", json.dumps(
    {"drive_id": "rootfs", "path_on_host": "/path/to/rootfs.ext4",
     "is_root_device": True, "is_read_only": False}))
api("PUT", "/vfio-device", json.dumps(
    {"host_bdf": HOST_BDF,
     "container_fd": container_fd, "group_fd": group_fd,
     "iommu_group_id": GROUP_ID}))
api("PUT", "/actions", json.dumps({"action_type": "InstanceStart"}))
```

Run as root (the `/dev/vfio/*` opens need it). On success, the VM boots
and the guest sees the device. For a GPU, expect to give the guest more
memory (≥4 GiB) and run `nvidia-smi` (or equivalent) inside it to confirm.

## What can go wrong

- **vfio-pci not loaded**: `modprobe vfio vfio_iommu_type1 vfio-pci`. The
  test scripts check this and tell you.
- **Group not viable**: another device in your IOMMU group isn't on
  vfio-pci. Either bind it too, or pick different hardware.
- **`/dev/vfio/<group>` missing**: the kernel hasn't materialized the
  group fd. Confirms vfio modules are loaded; check `dmesg` for IOMMU
  errors.
- **Guest doesn't see the device**: check the host BDF you passed and the
  guest BDF Porker reports in its log.
- **Guest sees the device, driver doesn't bind**: kernel-level. Check
  `dmesg` inside the guest. For NVIDIA, you need a guest rootfs with the
  nvidia driver and the GSP firmware.
- **Host crashes or splatters**: stop, file an issue. Don't iterate — DMA
  bugs corrupt host memory.

## What this doesn't cover

- Building Porker from source (see README).
- Building a guest rootfs with CUDA/nvidia drivers.
- Production deployment hardening. The jailer wraps everything in a
  chroot + capability drop; do that for any non-development use.
- Snapshot, hot-plug, balloon, multi-GPU. None are supported with VFIO.
