#!/usr/bin/env python3
"""
M1 VFIO passthrough proof script.

Opens VFIO fds in this process, inherits them into Firecracker via pass_fds,
configures the VM over the API, boots it, and checks that the device appears
in the guest.

Requirements on the host:
  - IOMMU enabled:    intel_iommu=on (or amd_iommu=on) in kernel cmdline
  - Modules loaded:   modprobe vfio vfio_iommu_type1 vfio-pci
  - Device bound:     echo <bdf> > /sys/bus/pci/drivers/vfio-pci/bind
  - Root access:      (for /dev/vfio/* and modprobe)

Requirements for the guest:
  - Kernel with CONFIG_PCI=y (most distro kernels work)
  - pciutils installed (lspci)
  - A simple init that at minimum prints something to ttyS0

Quick setup for an Intel X710 VF:
  # Create one VF on the PF:
  echo 1 > /sys/bus/pci/devices/0000:03:00.0/sriov_numvfs
  # Find the VF BDF (e.g. 0000:03:02.0):
  ls /sys/bus/pci/devices/0000:03:00.0/virtfn0 -la
  # Find its IOMMU group:
  ls /sys/bus/pci/devices/0000:03:02.0/iommu_group -la
  # Unbind from existing driver and bind to vfio-pci:
  echo 0000:03:02.0 > /sys/bus/pci/drivers/iavf/unbind
  echo 0000:03:02.0 > /sys/bus/pci/drivers/vfio-pci/bind

Usage:
  sudo python3 tools/test_vfio_m1.py \\
    --bdf 0000:03:02.0 \\
    --kernel /path/to/vmlinux \\
    --rootfs /path/to/rootfs.ext4 \\
    --firecracker ./build/cargo_target/x86_64-unknown-linux-musl/debug/firecracker
"""

import argparse
import http.client
import json
import os
import select
import socket
import subprocess
import sys
import time

API_SOCK = "/tmp/fc_vfio_test.sock"
FC_LOG   = "/tmp/fc_vfio_test.log"

# ── HTTP-over-Unix-socket ────────────────────────────────────────────────────

class _UnixConn(http.client.HTTPConnection):
    def __init__(self, sock_path):
        super().__init__("localhost")
        self._sock_path = sock_path

    def connect(self):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(self._sock_path)
        self.sock = s


def api(method, path, body=None):
    conn = _UnixConn(API_SOCK)
    headers = {"Content-Type": "application/json", "Accept": "application/json"}
    data = json.dumps(body).encode() if body is not None else None
    conn.request(method, path, body=data, headers=headers)
    resp = conn.getresponse()
    status = resp.status
    resp_body = resp.read()
    conn.close()
    return status, resp_body


def api_ok(method, path, body=None, *, label):
    status, resp_body = api(method, path, body)
    if status not in (200, 204):
        print(f"  FAIL {label}: HTTP {status}")
        print(f"       {resp_body.decode(errors='replace')}")
        return False
    print(f"  OK   {label}")
    return True


# ── Prerequisites ────────────────────────────────────────────────────────────

def find_iommu_group(bdf):
    """Return the IOMMU group ID for a PCI BDF, or None if not found."""
    link = f"/sys/bus/pci/devices/{bdf}/iommu_group"
    if os.path.exists(link):
        target = os.readlink(link)   # e.g. "../../../kernel/iommu_groups/42"
        return int(os.path.basename(target))
    return None


def check_prerequisites(bdf):
    ok = True

    if not os.path.exists("/sys/kernel/iommu_groups"):
        print("FAIL: IOMMU not enabled.")
        print("      Add intel_iommu=on (or amd_iommu=on) to GRUB_CMDLINE_LINUX and reboot.")
        ok = False

    if not os.path.exists("/sys/bus/pci/drivers/vfio-pci"):
        print("FAIL: vfio-pci driver not loaded.")
        print("      Run: modprobe vfio vfio_iommu_type1 vfio-pci")
        ok = False

    if not os.path.exists(f"/sys/bus/pci/devices/{bdf}"):
        print(f"FAIL: device {bdf} not found in sysfs.")
        ok = False
    else:
        driver_link = f"/sys/bus/pci/devices/{bdf}/driver"
        if os.path.exists(driver_link):
            driver = os.path.basename(os.readlink(driver_link))
            if driver != "vfio-pci":
                print(f"FAIL: {bdf} is bound to '{driver}', not vfio-pci.")
                print(f"      Run: echo {bdf} > /sys/bus/pci/drivers/{driver}/unbind")
                print(f"           echo vfio-pci > /sys/bus/pci/devices/{bdf}/driver_override")
                print(f"           echo {bdf} > /sys/bus/pci/drivers_probe")
                ok = False
        else:
            print(f"FAIL: {bdf} has no driver bound.")
            print(f"      Run: echo vfio-pci > /sys/bus/pci/devices/{bdf}/driver_override")
            print(f"           echo {bdf} > /sys/bus/pci/drivers_probe")
            ok = False

    if not ok:
        sys.exit(1)

    group_id = find_iommu_group(bdf)
    if group_id is None:
        print(f"FAIL: cannot determine IOMMU group for {bdf}")
        sys.exit(1)

    group_dev = f"/dev/vfio/{group_id}"
    if not os.path.exists(group_dev):
        print(f"FAIL: {group_dev} does not exist (is the vfio module loaded?)")
        sys.exit(1)

    if not os.path.exists("/dev/vfio/vfio"):
        print("FAIL: /dev/vfio/vfio does not exist.")
        print("      Run: modprobe vfio")
        sys.exit(1)

    print(f"  prerequisites OK: {bdf} → IOMMU group {group_id} → {group_dev}")
    return group_id


def wait_for_socket(path, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(path):
            return True
        time.sleep(0.05)
    return False


# ── Main ─────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="M1 VFIO passthrough proof: boots a Firecracker VM with a real PCI device.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--bdf",         required=True,
                        help="PCI BDF of the device to pass through, e.g. 0000:03:02.0")
    parser.add_argument("--kernel",      required=True,
                        help="Path to vmlinux or bzImage guest kernel")
    parser.add_argument("--rootfs",      required=True,
                        help="Path to guest rootfs ext4 image (must have pciutils/lspci)")
    parser.add_argument("--firecracker", default="./firecracker",
                        help="Path to Firecracker binary (default: ./firecracker)")
    parser.add_argument("--mem-mib",     type=int, default=512,
                        help="Guest memory in MiB (default: 512)")
    parser.add_argument("--vcpus",       type=int, default=2,
                        help="Guest vCPU count (default: 2)")
    parser.add_argument("--boot-args",   default="",
                        help="Extra kernel boot args (appended to defaults)")
    args = parser.parse_args()

    print("=== M1 VFIO Proof Test ===\n")

    # ── 1. Prerequisites ──────────────────────────────────────────────────────
    print("[1] Checking prerequisites...")
    group_id = check_prerequisites(args.bdf)

    # ── 2. Open VFIO fds ─────────────────────────────────────────────────────
    print("\n[2] Opening VFIO file descriptors...")
    container_fd = os.open("/dev/vfio/vfio", os.O_RDWR)
    group_fd     = os.open(f"/dev/vfio/{group_id}", os.O_RDWR)
    print(f"  /dev/vfio/vfio          → fd {container_fd}")
    print(f"  /dev/vfio/{group_id:<14} → fd {group_fd}")

    # Clean up stale socket
    try:
        os.unlink(API_SOCK)
    except FileNotFoundError:
        pass

    # ── 3. Start Firecracker ─────────────────────────────────────────────────
    print(f"\n[3] Starting Firecracker (log: {FC_LOG})...")
    fc_cmd = [
        args.firecracker,
        "--api-sock",  API_SOCK,
        "--enable-pci",
        "--log-path",  FC_LOG,
        "--level",     "Debug",
    ]
    # pass_fds ensures the VFIO fds survive across exec into Firecracker
    fc_proc = subprocess.Popen(
        fc_cmd,
        pass_fds=(container_fd, group_fd),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    # Close in parent — Firecracker now owns these fds
    os.close(container_fd)
    os.close(group_fd)

    if not wait_for_socket(API_SOCK, timeout=5.0):
        print(f"FAIL: API socket {API_SOCK} did not appear within 5s")
        print(f"      Check {FC_LOG} for details")
        fc_proc.kill()
        sys.exit(1)
    print(f"  Firecracker ready (pid {fc_proc.pid})")

    # ── 4. Configure VM ───────────────────────────────────────────────────────
    print("\n[4] Configuring VM via API...")

    boot_args = " ".join(filter(None, [
        "console=ttyS0",
        "reboot=k",
        "panic=1",
        "pci=noaer,realloc=off",
        "nokaslr",
        "root=/dev/vda",
        "rw",
        args.boot_args,
    ]))

    all_ok = True
    all_ok &= api_ok("PUT", "/machine-config",
                     {"vcpu_count": args.vcpus, "mem_size_mib": args.mem_mib},
                     label="machine-config")
    all_ok &= api_ok("PUT", "/boot-source",
                     {"kernel_image_path": args.kernel, "boot_args": boot_args},
                     label="boot-source")
    all_ok &= api_ok("PUT", "/drives/rootfs",
                     {"drive_id": "rootfs", "path_on_host": args.rootfs,
                      "is_root_device": True, "is_read_only": False},
                     label="rootfs drive")
    all_ok &= api_ok("PUT", "/vfio-device",
                     {"host_bdf":        args.bdf,
                      "container_fd":    container_fd,   # fd number valid in Firecracker
                      "group_fd":        group_fd,
                      "iommu_group_id":  group_id},
                     label=f"vfio-device ({args.bdf})")

    if not all_ok:
        print("\nFAIL: VM configuration failed. Check output above.")
        fc_proc.terminate()
        sys.exit(1)

    # ── 5. Boot ───────────────────────────────────────────────────────────────
    print("\n[5] Starting VM...")
    if not api_ok("PUT", "/actions", {"action_type": "InstanceStart"}, label="InstanceStart"):
        fc_proc.terminate()
        sys.exit(1)

    # ── 6. Watch serial output ────────────────────────────────────────────────
    print("\n[6] Serial console output (Ctrl-C to stop):\n")
    print("-" * 60)

    found_login  = False
    found_lspci  = False
    found_device = False
    buffer = b""
    deadline = time.monotonic() + 9999999999#60.0

    try:
        while time.monotonic() < deadline:
            r, _, _ = select.select([fc_proc.stdout], [], [], 0.2)
            if r:
                chunk = fc_proc.stdout.read1(4096)
                if not chunk:
                    break
                buffer += chunk
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()

            # Detect guest boot milestones in the accumulated output
            if b"login:" in buffer or b"bash" in buffer or b"#" in buffer:
                found_login = True
            if b"lspci" in buffer or b"PCI" in buffer:
                found_lspci = True
            # Look for NIC-class devices (Ethernet or Network controller)
            if b"Ethernet" in buffer or b"Network controller" in buffer:
                found_device = True

            if fc_proc.poll() is not None:
                break

    except KeyboardInterrupt:
        print("\n(interrupted)")

    print("\n" + "-" * 60)

    # ── 7. Report ─────────────────────────────────────────────────────────────
    print("\n=== Results ===")
    print(f"  Guest boot reached login/shell : {'YES' if found_login  else 'no (check kernel/rootfs)'}")
    print(f"  PCI output seen in serial      : {'YES' if found_lspci  else 'no'}")
    print(f"  Ethernet/NIC device detected   : {'YES' if found_device else 'no'}")

    if found_device:
        print("\nPASS: Device appeared in guest! ✓")
        print("      Next: SSH in and run 'lspci -v' and 'ip link show' to confirm full function.")
    elif found_lspci:
        print("\nPASS (partial): lspci ran but no NIC line seen.")
        print("      Try: lspci -v inside guest to check device class.")
    elif found_login:
        print("\nPARTIAL: Guest booted but no PCI output captured.")
        print("      SSH into guest and run: lspci")
        print(f"      Full debug log: {FC_LOG}")
    else:
        print("\nFAIL: Guest did not reach login prompt.")
        print(f"      Check {FC_LOG} and serial output above.")

    # ── 8. Cleanup ────────────────────────────────────────────────────────────
    print("\nStopping Firecracker...")
    fc_proc.terminate()
    try:
        fc_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        fc_proc.kill()
    try:
        os.unlink(API_SOCK)
    except FileNotFoundError:
        pass


if __name__ == "__main__":
    main()
