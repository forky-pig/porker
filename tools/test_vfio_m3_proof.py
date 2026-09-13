#!/usr/bin/env python3
"""
M3 VFIO passthrough — full proof script.

Three subcommands, analogous to test_vfio_m2_proof.py:

  host-baseline  Capture host-side reference values via nvidia-smi.  Requires
                 the GPU to be bound to the nvidia driver on the host (NOT
                 vfio-pci).  See project_m3_milestone.md for the rebind dance.
                 Writes /tmp/vfio_m3_host_baseline.json.

  stability      Boot the VM N times in a row.  Each boot must reach login and
                 exit cleanly (exercises FLR-on-drop and KVM slot cleanup with
                 the GPU's real state).

  compare        Parse guest-probe output (VFIO_M3_PROBE_START/END block) and
                 diff the nvidia-smi fields against the host baseline.  Reports
                 PASS/FAIL per metric.

Typical flow:
  # 1. Rebind GPU to nvidia on the host (temporarily):
  #    echo 0000:01:00.0 > /sys/bus/pci/drivers/vfio-pci/unbind
  #    echo 0000:01:00.1 > /sys/bus/pci/drivers/vfio-pci/unbind
  #    echo 0000:01:00.0 > /sys/bus/pci/drivers/nvidia/bind
  #    echo 0000:01:00.1 > /sys/bus/pci/drivers/snd_hda_intel/bind   # audio
  #    nvidia-smi -q    # verify
  sudo python3 tools/test_vfio_m3_proof.py host-baseline --bdf 0000:01:00.0

  # 2. Rebind to vfio-pci (reverse of step 1).

  # 3. Stability — 3 boots, clean teardown each time
  sudo python3 tools/test_vfio_m3_proof.py stability --runs 3 \\
    --bdf 0000:01:00.0 --kernel ./vmlinux-host --rootfs ./rootfs.ext4 \\
    --firecracker ./build/cargo_target/debug/firecracker

  # 4. Boot interactively with test_vfio_m3.py, run the guest probe, capture:
  #    sh test_vfio_m3_guest_probe.sh > /tmp/guest_output.txt
  sudo python3 tools/test_vfio_m3_proof.py compare \\
    --guest-output /tmp/guest_output.txt
"""

import argparse
import http.client
import json
import os
import re
import select
import socket
import subprocess
import sys
import time

API_SOCK        = "/tmp/fc_vfio_m3_proof.sock"
FC_LOG_TEMPLATE = "/tmp/fc_vfio_m3_proof_run{}.log"
BASELINE_FILE   = "/tmp/vfio_m3_host_baseline.json"


# ── HTTP over Unix socket ────────────────────────────────────────────────────

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
        print(f"    FAIL {label}: HTTP {status}: {resp_body.decode(errors='replace')}")
        return False
    return True


# ── nvidia-smi parser (shared by host-baseline and compare) ─────────────────

# Map of proof-critical fields we care about.  The regex captures the value
# column from nvidia-smi -q's "Key : Value" layout.  Keys are not unique across
# the entire dump (e.g. "Total" appears under FB Memory AND BAR1 Memory), so we
# have to parse sectioned; we scope by the preceding section header.
def parse_nvidia_smi(text):
    """
    Extract M3-relevant fields from `nvidia-smi -q` output.  Returns a dict
    of values as strings (no unit conversion), or None if nothing usable.
    """
    out = {}

    # Driver version (top-level line)
    m = re.search(r"Driver Version\s*:\s*(\S+)", text)
    if m: out["driver_version"] = m.group(1)

    # Per-GPU fields — take the first GPU block (M3 is single-GPU).
    m = re.search(r"Product Name\s*:\s*(.+?)\s*$", text, re.MULTILINE)
    if m: out["product_name"] = m.group(1).strip()

    m = re.search(r"Product Architecture\s*:\s*(.+?)\s*$", text, re.MULTILINE)
    if m: out["product_architecture"] = m.group(1).strip()

    m = re.search(r"VBIOS Version\s*:\s*(\S+)", text)
    if m: out["vbios_version"] = m.group(1)

    m = re.search(r"GPU UUID\s*:\s*(\S+)", text)
    if m: out["gpu_uuid"] = m.group(1)

    m = re.search(r"GSP Firmware Version\s*:\s*(\S+)", text)
    if m: out["gsp_firmware_version"] = m.group(1)

    m = re.search(r"Virtualization Mode\s*:\s*(\S+)", text)
    if m: out["virtualization_mode"] = m.group(1)

    # FB Memory — scoped to that section.
    m = re.search(r"FB Memory Usage.*?Total\s*:\s*(\d+)\s*MiB", text, re.DOTALL)
    if m: out["fb_memory_total_mib"] = int(m.group(1))

    # BAR1 Memory — scoped.
    m = re.search(r"BAR1 Memory Usage.*?Total\s*:\s*(\d+)\s*MiB", text, re.DOTALL)
    if m: out["bar1_memory_total_mib"] = int(m.group(1))

    # PCIe current generation and width
    m = re.search(r"PCIe Generation.*?Current\s*:\s*(\d+)", text, re.DOTALL)
    if m: out["pcie_gen_current"] = int(m.group(1))

    m = re.search(r"Link Width.*?Current\s*:\s*(\S+)", text, re.DOTALL)
    if m: out["pcie_link_width_current"] = m.group(1)

    return out if out else None


# ── Host baseline capture ───────────────────────────────────────────────────

def cmd_host_baseline(args):
    bdf = args.bdf
    print(f"=== Host baseline capture for {bdf} ===\n")

    # Sanity: is the GPU bound to nvidia (not vfio-pci)?
    driver_link = f"/sys/bus/pci/devices/{bdf}/driver"
    driver = os.path.basename(os.readlink(driver_link)) if os.path.exists(driver_link) else None
    if driver != "nvidia":
        print(f"FAIL: {bdf} is bound to '{driver}', not 'nvidia'.")
        print(f"      Rebind to the nvidia driver before capturing baseline:")
        print(f"        echo {bdf} > /sys/bus/pci/drivers/{driver}/unbind")
        print(f"        echo {bdf} > /sys/bus/pci/drivers/nvidia/bind")
        sys.exit(1)

    # Run nvidia-smi and capture its full -q output.
    try:
        result = subprocess.run(
            ["nvidia-smi", "-q"],
            check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
    except subprocess.CalledProcessError as e:
        print(f"FAIL: nvidia-smi -q returned {e.returncode}")
        print(e.stderr.decode(errors="replace"))
        sys.exit(1)
    except FileNotFoundError:
        print("FAIL: nvidia-smi not found on host.")
        sys.exit(1)

    smi_text = result.stdout.decode()
    fields = parse_nvidia_smi(smi_text)
    if not fields:
        print("FAIL: couldn't parse any fields from nvidia-smi output.")
        sys.exit(1)

    out = {
        "bdf": bdf,
        "captured_at": time.time(),
        "raw_nvidia_smi": smi_text,
        "fields": fields,
    }

    # Print a summary to console
    print(f"  Driver version       : {fields.get('driver_version', '?')}")
    print(f"  Product name         : {fields.get('product_name', '?')}")
    print(f"  Architecture         : {fields.get('product_architecture', '?')}")
    print(f"  VBIOS version        : {fields.get('vbios_version', '?')}")
    print(f"  GPU UUID             : {fields.get('gpu_uuid', '?')}")
    print(f"  GSP firmware         : {fields.get('gsp_firmware_version', '?')}")
    print(f"  FB memory (total)    : {fields.get('fb_memory_total_mib', '?')} MiB")
    print(f"  BAR1 memory (total)  : {fields.get('bar1_memory_total_mib', '?')} MiB")
    print(f"  PCIe gen (current)   : {fields.get('pcie_gen_current', '?')}")
    print(f"  PCIe width (current) : {fields.get('pcie_link_width_current', '?')}")

    with open(BASELINE_FILE, "w") as f:
        json.dump(out, f, indent=2)
    print(f"\n  Baseline written to {BASELINE_FILE}")


# ── Stability (N boots) ─────────────────────────────────────────────────────

def find_iommu_group(bdf):
    link = f"/sys/bus/pci/devices/{bdf}/iommu_group"
    if os.path.exists(link):
        return int(os.path.basename(os.readlink(link)))
    return None


def wait_for_socket(path, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(path):
            return True
        time.sleep(0.05)
    return False


def run_single_boot(args, run_idx, group_id):
    """Boot the VM once, wait for login, SIGTERM, verify teardown."""
    log_path = FC_LOG_TEMPLATE.format(run_idx)
    try:
        os.unlink(API_SOCK)
    except FileNotFoundError:
        pass
    try:
        os.unlink(log_path)
    except FileNotFoundError:
        pass

    # Re-open VFIO fds each iteration — proves they can be re-opened after FLR.
    container_fd = os.open("/dev/vfio/vfio", os.O_RDWR)
    group_fd     = os.open(f"/dev/vfio/{group_id}", os.O_RDWR)

    fc_cmd = [
        args.firecracker, "--api-sock", API_SOCK,
        "--enable-pci", "--log-path", log_path, "--level", "Debug",
    ]
    fc_proc = subprocess.Popen(
        fc_cmd,
        pass_fds=(container_fd, group_fd),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    os.close(container_fd)
    os.close(group_fd)

    if not wait_for_socket(API_SOCK, timeout=5.0):
        fc_proc.kill()
        return {"ok": False, "stage": "api-socket", "log": log_path}

    boot_args = (
        "console=ttyS0 reboot=k panic=1 pci=noaer,realloc=off nokaslr "
        "nomodeset nouveau.modeset=0 root=/dev/vda rw"
    )
    cfg_ok = True
    cfg_ok &= api_ok("PUT", "/machine-config",
                     {"vcpu_count": args.vcpus, "mem_size_mib": args.mem_mib},
                     label="machine-config")
    cfg_ok &= api_ok("PUT", "/boot-source",
                     {"kernel_image_path": args.kernel, "boot_args": boot_args},
                     label="boot-source")
    cfg_ok &= api_ok("PUT", "/drives/rootfs",
                     {"drive_id": "rootfs", "path_on_host": args.rootfs,
                      "is_root_device": True, "is_read_only": False},
                     label="rootfs")
    cfg_ok &= api_ok("PUT", "/vfio-device",
                     {"host_bdf":       args.bdf,
                      "container_fd":   container_fd,
                      "group_fd":       group_fd,
                      "iommu_group_id": group_id},
                     label="vfio-device")
    if not cfg_ok:
        fc_proc.terminate()
        try: fc_proc.wait(timeout=5)
        except subprocess.TimeoutExpired: fc_proc.kill()
        return {"ok": False, "stage": "api-config", "log": log_path}

    if not api_ok("PUT", "/actions", {"action_type": "InstanceStart"}, label="start"):
        fc_proc.terminate()
        try: fc_proc.wait(timeout=5)
        except subprocess.TimeoutExpired: fc_proc.kill()
        return {"ok": False, "stage": "instance-start", "log": log_path}

    # Wait for shell (up to 60s — nvidia init is slower than M1's NIC).
    buffer = b""
    deadline = time.monotonic() + 60
    reached_shell = False
    while time.monotonic() < deadline:
        r, _, _ = select.select([fc_proc.stdout], [], [], 0.2)
        if r:
            chunk = fc_proc.stdout.read1(4096)
            if not chunk:
                break
            buffer += chunk
        if b"login:" in buffer or b"# " in buffer:
            reached_shell = True
            break
        if fc_proc.poll() is not None:
            break

    if not reached_shell:
        fc_proc.kill()
        # Look for a panic-cause line specifically.  Kernel panic tails often
        # scroll past the actual cause ("Unable to mount root fs", "VFS:",
        # "Kernel panic - not syncing: <reason>") — try to surface it explicitly.
        text = buffer.decode(errors="replace")
        cause = None
        for pat in [r"Unable to mount root fs[^\n]*",
                    r"VFS: [^\n]*",
                    r"Kernel panic - not syncing: [^\n]*"]:
            m = re.search(pat, text)
            if m:
                cause = m.group(0).strip()
                break
        return {"ok": False, "stage": "shell-wait", "log": log_path,
                "tail": buffer[-2048:].decode(errors="replace"),
                "cause": cause}

    # Clean shutdown — exercises VfioPciDevice::drop (FLR + slot release + IOMMU unmap).
    fc_proc.terminate()
    try:
        rc = fc_proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        fc_proc.kill()
        return {"ok": False, "stage": "sigterm-timeout", "log": log_path}

    # Scan log for VFIO / seccomp failures.
    errors = []
    if os.path.exists(log_path):
        with open(log_path) as f:
            for line in f:
                if re.search(r"(ERROR|FAIL).*vfio|VFIO.*fail|seccomp|SIGSYS",
                             line, re.IGNORECASE):
                    errors.append(line.rstrip())

    # GPU still in sysfs (not wedged)?
    gpu_sysfs = f"/sys/bus/pci/devices/{args.bdf}"
    if not os.path.exists(gpu_sysfs):
        return {"ok": False, "stage": "gpu-sysfs-vanished", "log": log_path}

    return {
        "ok": not errors,
        "stage": "complete",
        "rc": rc,
        "log": log_path,
        "errors": errors,
    }


def cmd_stability(args):
    print(f"=== Stability test: {args.runs} back-to-back boots ===\n")

    group_id = find_iommu_group(args.bdf)
    if group_id is None:
        print(f"FAIL: cannot determine IOMMU group for {args.bdf}")
        sys.exit(1)

    results = []
    for i in range(1, args.runs + 1):
        print(f"[boot {i}/{args.runs}]")
        r = run_single_boot(args, i, group_id)
        results.append(r)
        status = "OK" if r["ok"] else "FAIL"
        print(f"  {status}  stage={r['stage']}  log={r['log']}")
        if r.get("errors"):
            for e in r["errors"]:
                print(f"    log-error: {e}")
        if r.get("cause"):
            print(f"    cause: {r['cause']}")
        if r.get("tail"):
            # Print the last ~12 lines — enough to see panic context
            tail_lines = r["tail"].rstrip().splitlines()[-12:]
            print(f"    tail:")
            for line in tail_lines:
                print(f"      {line}")
        time.sleep(1.0)  # Let the kernel settle FLR between runs.

    passed = sum(1 for r in results if r["ok"])
    print(f"\n=== Stability summary: {passed}/{args.runs} passed ===")
    if passed < args.runs:
        sys.exit(1)


# ── Compare guest probe to host baseline ────────────────────────────────────

def parse_guest_probe(text):
    """Extract structured fields from the VFIO_M3_PROBE_START/END block."""
    m = re.search(r"=== VFIO_M3_PROBE_START ===(.+?)=== VFIO_M3_PROBE_END ===",
                  text, re.DOTALL)
    if not m:
        return None
    body = m.group(1)

    out = {}

    # DRIVER_BIND section — parse the DRIVER_NAME= line
    m = re.search(r"DRIVER_NAME=(\S+)", body)
    if m: out["driver_name"] = m.group(1)

    # Also look at `Kernel driver in use:` from lspci -k
    m = re.search(r"Kernel driver in use:\s*(\S+)", body)
    if m: out["lspci_driver"] = m.group(1)

    # DEVICE_NODES — list every /dev/nvidia* line seen.
    # The `ls` output in this section can contain a blank line when `ls` recurses
    # into /dev/nvidia-caps/, so we use [\s\S] (matches newlines) instead of .+\n
    # which fails on blank lines.
    m = re.search(r"--- DEVICE_NODES ---\s*\n([\s\S]+?)\n---", body)
    if m:
        node_lines = [l for l in m.group(1).splitlines() if "/dev/nvidia" in l]
        out["device_nodes"] = node_lines

    # PCI_COMMAND — raw COMMAND bytes "07 00" style
    m = re.search(r"COMMAND=0x([0-9a-fA-F]+)", body)
    if m:
        # COMMAND is stored little-endian; first byte is the low byte.
        raw = m.group(1)
        # Normalise: join hex bytes; M2 script does same
        out["pci_command_raw"] = raw
        # Low byte = first 2 hex chars after stripping spaces
        cleaned = raw.replace(" ", "")
        if len(cleaned) >= 2:
            low = int(cleaned[:2], 16)
            out["pci_command_bus_master"] = bool(low & 0x04)
            out["pci_command_mem_enable"] = bool(low & 0x02)

    # Parse nvidia-smi output embedded in the probe block
    smi_match = re.search(r"--- NVIDIA_SMI ---\s*\n(.+?)\n--- DMESG_NVRM ---",
                          body, re.DOTALL)
    if smi_match:
        smi_text = smi_match.group(1)
        parsed = parse_nvidia_smi(smi_text)
        if parsed:
            out["nvidia_smi"] = parsed
        elif "NVIDIA_SMI_UNAVAILABLE" in smi_text:
            out["nvidia_smi_unavailable"] = True

    # DMESG errors — flag anything scary
    dmesg_match = re.search(r"--- DMESG_NVRM ---\s*\n(.+)$", body, re.DOTALL)
    if dmesg_match:
        dmesg = dmesg_match.group(1)
        errs = []
        for pattern in [
            r"Direct firmware load.*failed",
            r"RmInitAdapter failed",
            r"signature.*verification failed",
            r"GPU.*Excluded.*Yes",
        ]:
            if re.search(pattern, dmesg, re.IGNORECASE):
                errs.append(pattern)
        out["dmesg_errors"] = errs

    return out


def cmd_compare(args):
    print("=== Compare guest probe vs host baseline ===\n")

    with open(args.guest_output) as f:
        guest_text = f.read()

    guest = parse_guest_probe(guest_text)
    if guest is None:
        print("FAIL: could not find VFIO_M3_PROBE_START/END block in guest output.")
        print("      Make sure the guest probe output was captured correctly.")
        sys.exit(1)

    # Baseline is optional.  If absent, we still validate all guest-side proof
    # criteria — only the host↔guest field cross-checks get skipped.
    baseline_present = os.path.exists(BASELINE_FILE)
    host_fields = {}
    if baseline_present:
        with open(BASELINE_FILE) as f:
            host = json.load(f)
        host_fields = host.get("fields", {})
        print(f"  Host baseline : {BASELINE_FILE}")
    else:
        print(f"  Host baseline : not present  (run 'host-baseline' for host↔guest cross-checks)")
    print(f"  Guest probe   : {args.guest_output}\n")

    checks = []

    # ── M3.1 Driver bound ─────────────────────────────────────────────────────
    driver = guest.get("driver_name") or guest.get("lspci_driver")
    checks.append((
        "M3.1 Driver bound",
        driver == "nvidia",
        f"driver_name={driver!r}",
    ))

    # ── M3.2 Device nodes exist ───────────────────────────────────────────────
    nodes = guest.get("device_nodes", [])
    has_nvidia0   = any("/dev/nvidia0"   in n for n in nodes)
    has_nvidiactl = any("/dev/nvidiactl" in n for n in nodes)
    checks.append((
        "M3.2 Device nodes exist",
        has_nvidia0 and has_nvidiactl,
        f"nvidia0={has_nvidia0}, nvidiactl={has_nvidiactl}, total_nodes={len(nodes)}",
    ))

    # ── M3.3 nvidia-smi reported real data ────────────────────────────────────
    smi = guest.get("nvidia_smi", {})
    if guest.get("nvidia_smi_unavailable"):
        checks.append(("M3.3 nvidia-smi query succeeded", False,
                       "nvidia-smi not installed in the guest"))
    elif not smi:
        checks.append(("M3.3 nvidia-smi query succeeded", False,
                       "nvidia-smi present but produced no parseable fields"))
    elif baseline_present:
        # Full host↔guest cross-check.
        pname_match = smi.get("product_name") == host_fields.get("product_name")
        arch_match  = smi.get("product_architecture") == host_fields.get("product_architecture")
        vbios_match = smi.get("vbios_version") == host_fields.get("vbios_version")
        fb_match    = smi.get("fb_memory_total_mib") == host_fields.get("fb_memory_total_mib")
        bar1_match  = smi.get("bar1_memory_total_mib") == host_fields.get("bar1_memory_total_mib")
        uuid_match  = smi.get("gpu_uuid") == host_fields.get("gpu_uuid")

        all_matched = all([pname_match, arch_match, vbios_match, fb_match, bar1_match, uuid_match])
        detail_lines = [
            f"product_name:         {'=' if pname_match else '≠'} host={host_fields.get('product_name')!r} guest={smi.get('product_name')!r}",
            f"architecture:         {'=' if arch_match  else '≠'} host={host_fields.get('product_architecture')!r} guest={smi.get('product_architecture')!r}",
            f"vbios_version:        {'=' if vbios_match else '≠'} host={host_fields.get('vbios_version')!r} guest={smi.get('vbios_version')!r}",
            f"fb_memory_total_mib:  {'=' if fb_match    else '≠'} host={host_fields.get('fb_memory_total_mib')} guest={smi.get('fb_memory_total_mib')}",
            f"bar1_memory_total_mib:{'=' if bar1_match  else '≠'} host={host_fields.get('bar1_memory_total_mib')} guest={smi.get('bar1_memory_total_mib')}",
            f"gpu_uuid:             {'=' if uuid_match  else '≠'} host={host_fields.get('gpu_uuid')!r} guest={smi.get('gpu_uuid')!r}",
        ]
        checks.append(("M3.3 nvidia-smi fields match host baseline",
                       all_matched,
                       "\n           " + "\n           ".join(detail_lines)))
    else:
        # Validate-mode: no baseline — sanity check that guest values are plausible.
        pname_ok   = bool(smi.get("product_name")) and "NVIDIA" in (smi.get("product_name") or "").upper()
        arch_ok    = smi.get("product_architecture") in {
            "Ada Lovelace", "Hopper", "Blackwell", "Ampere", "Turing", "Volta", "Pascal",
        }
        vbios_ok   = bool(smi.get("vbios_version")) and "?" not in (smi.get("vbios_version") or "?")
        fb_ok      = (smi.get("fb_memory_total_mib") or 0) > 0
        bar1_ok    = (smi.get("bar1_memory_total_mib") or 0) > 0
        uuid_ok    = (smi.get("gpu_uuid") or "").startswith("GPU-")
        pcie_ok    = (smi.get("pcie_gen_current") or 0) >= 3  # Gen3+ = reasonable for any target GPU

        all_ok = all([pname_ok, arch_ok, vbios_ok, fb_ok, bar1_ok, uuid_ok, pcie_ok])
        detail_lines = [
            f"product_name:         {'ok' if pname_ok else '!!'} guest={smi.get('product_name')!r}",
            f"architecture:         {'ok' if arch_ok  else '!!'} guest={smi.get('product_architecture')!r}",
            f"vbios_version:        {'ok' if vbios_ok else '!!'} guest={smi.get('vbios_version')!r}",
            f"fb_memory_total_mib:  {'ok' if fb_ok    else '!!'} guest={smi.get('fb_memory_total_mib')}",
            f"bar1_memory_total_mib:{'ok' if bar1_ok  else '!!'} guest={smi.get('bar1_memory_total_mib')}",
            f"gpu_uuid:             {'ok' if uuid_ok  else '!!'} guest={smi.get('gpu_uuid')!r}",
            f"pcie_gen_current:     {'ok' if pcie_ok  else '!!'} guest={smi.get('pcie_gen_current')}",
        ]
        checks.append(("M3.3 nvidia-smi values plausible (no-baseline mode)",
                       all_ok,
                       "\n           " + "\n           ".join(detail_lines)))

    # ── M3.3b Virtualization mode (guest-only; no baseline needed) ────────────
    if smi and not guest.get("nvidia_smi_unavailable"):
        vmode = smi.get("virtualization_mode")
        checks.append(("M3.3b Virtualization mode correct",
                       vmode == "Pass-Through",
                       f"guest virtualization_mode={vmode!r} (expected Pass-Through)"))

        # ── M3.3c GSP firmware == driver version (intra-guest consistency) ──
        drv = smi.get("driver_version")
        gsp = smi.get("gsp_firmware_version")
        gsp_match = drv is not None and drv == gsp
        checks.append(("M3.3c GSP firmware version == driver version",
                       gsp_match,
                       f"driver={drv!r} gsp={gsp!r}"))

    # 4. No crashes / errors in dmesg
    dmesg_errs = guest.get("dmesg_errors", [])
    checks.append((
        "M3.4 No NVRM/firmware errors in guest dmesg",
        not dmesg_errs,
        "clean" if not dmesg_errs else f"patterns matched: {dmesg_errs}",
    ))

    # Secondary: Bus Master bit set (plan §2 secondary)
    if "pci_command_bus_master" in guest:
        checks.append((
            "M3.S1 Bus Master bit set (post driver bind)",
            guest["pci_command_bus_master"],
            f"COMMAND raw=0x{guest.get('pci_command_raw', '?')} BME={guest['pci_command_bus_master']} MemEn={guest.get('pci_command_mem_enable')}",
        ))

    # Print results
    all_ok = True
    for name, ok, detail in checks:
        tag = "PASS" if ok else "FAIL"
        print(f"  [{tag}]  {name}")
        print(f"          {detail}")
        if not ok:
            all_ok = False

    print()
    if all_ok:
        print("=== M3 PROOF: all checks passed ===")
    else:
        print("=== M3 PROOF: one or more checks FAILED ===")
        sys.exit(1)


# ── Main ────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="M3 full proof script.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    sub = parser.add_subparsers(dest="command", required=True)

    b = sub.add_parser("host-baseline", help="capture host nvidia-smi baseline")
    b.add_argument("--bdf", required=True, help="PCI BDF, e.g. 0000:01:00.0")

    s = sub.add_parser("stability", help="boot the VM N times in a row")
    s.add_argument("--bdf", required=True)
    s.add_argument("--kernel", required=True)
    s.add_argument("--rootfs", required=True)
    s.add_argument("--firecracker", default="./firecracker")
    s.add_argument("--runs", type=int, default=3)
    s.add_argument("--mem-mib", type=int, default=4096)
    s.add_argument("--vcpus",   type=int, default=2)

    c = sub.add_parser("compare", help="diff guest probe output vs host baseline")
    c.add_argument("--guest-output", required=True,
                   help="file containing the guest-probe output "
                        "(VFIO_M3_PROBE_START/END block)")

    args = parser.parse_args()

    if args.command == "host-baseline":
        cmd_host_baseline(args)
    elif args.command == "stability":
        cmd_stability(args)
    elif args.command == "compare":
        cmd_compare(args)


if __name__ == "__main__":
    main()
