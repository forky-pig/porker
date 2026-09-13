#!/bin/bash
# Porker hardware round: prove VFIO passthrough still works on the binary you just built.
# Part of the upstream-sync merge process (docs/UPSTREAM.md). Run as root:
#
#   sudo tools/porker_hw_round.sh gpu     # GPU passthrough: 3x boot + clean teardown (M3 stability harness)
#   sudo tools/porker_hw_round.sh nic     # NIC passthrough: guest driver binds the card (M1 harness)
#   sudo tools/porker_hw_round.sh all
#
# Binds the device(s) to vfio-pci, runs the existing proof harness against the release binary,
# then rebinds the device(s) to their host drivers. Logs land in build/hw-<what>-<timestamp>.log.
#
# Defaults below match the dev laptop (dash); override any of them in the environment, e.g.
#   sudo GPU=0000:65:00.0 GPU_AUDIO= NIC=0000:03:00.0 NIC_MAC=aa:bb:.. tools/porker_hw_round.sh all
#   sudo FC_BIN=/path/to/firecracker tools/porker_hw_round.sh nic      # A/B against another build
#   sudo KILL_GPU_HOLDERS=1 tools/porker_hw_round.sh gpu               # kill e.g. a browser's decoder holding the GPU
#
# Lessons baked in (each cost real time once): modprobe -a; never wrap Firecracker in plain
# `timeout` (background process group -> SIGTTOU -> silently stopped VMM); stdin from /dev/null;
# systemd.run needs run_success_action=none or the guest powers off; Ubuntu's kernel.printk sysctl
# hides INFO-level probe lines, so read them back with dmesg.
set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
FC=${FC_BIN:-$REPO/build/cargo_target/release/firecracker}   # override: sudo FC_BIN=/path/to/firecracker bash build/hw-round.sh nic
GPU=${GPU:-0000:01:00.0}
GPU_AUDIO=${GPU_AUDIO-0000:01:00.1}   # GPU audio function in the same IOMMU group; set empty if there is none
NIC=${NIC:-0000:76:00.0}
NIC_MAC=${NIC_MAC:-74:5d:22:f7:9a:4a}            # hardware MAC of $NIC: proves the guest driver bound the real card
NIC_KERNEL=${NIC_KERNEL:-vmlinux-host}            # guest kernel whose modules exist in NIC_ROOTFS
NIC_ROOTFS=${NIC_ROOTFS:-rootfs.ext4}
NIC_GUEST_KVER=${NIC_GUEST_KVER:-6.17.9-76061709-generic}
GPU_KERNEL=${GPU_KERNEL:-gpu-kernel-build/out/vmlinux}
GPU_ROOTFS=${GPU_ROOTFS:-rootfs-gpu-b.ext4}
WHAT=${1:-all}

cd "$REPO" || exit 1
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }
# NB: without -a, modprobe treats the 2nd+ words as *parameters* of the first module.
modprobe -a vfio vfio_iommu_type1 vfio-pci || { echo "!! modprobe of the vfio modules failed"; exit 1; }
[ -d /sys/bus/pci/drivers/vfio-pci ] || { echo "!! vfio-pci driver is not registered after modprobe (lsmod | grep vfio)"; exit 1; }
[ -e /dev/vfio/vfio ] || { echo "!! /dev/vfio/vfio missing"; exit 1; }

cur_driver() { local l=/sys/bus/pci/devices/$1/driver; [ -L "$l" ] && basename "$(readlink -f "$l")"; }

# Print anything holding the given device nodes open; return 1 if there is something.
# (An unbind blocks silently until every open handle is gone - this is the usual cause of a "hang".)
holders() {
    local out; out=$(fuser -v "$@" 2>&1 | grep -vE '^\s*USER|does not exist|^$')
    [ -z "$out" ] && return 0
    echo "$out"; return 1
}

to_vfio() {
    local dev=$1 drv; drv=$(cur_driver "$dev")
    [ "$drv" = vfio-pci ] && return 0
    if [ -n "$drv" ]; then
        echo "   unbinding $dev from $drv ..."
        ( echo "$dev" > "/sys/bus/pci/drivers/$drv/unbind" ) & local w=$! t=0
        while kill -0 "$w" 2>/dev/null; do
            sleep 1; t=$((t+1))
            if [ "$t" = 10 ]; then
                echo "!! unbind of $dev from $drv has been blocked for 10s: something still has it open."
                case $drv in
                    nvidia) holders /dev/nvidia0 /dev/nvidiactl /dev/nvidia-uvm /dev/nvidia-modeset ;;
                    snd_hda_intel)
                        local c; c=$(ls -d "/sys/bus/pci/devices/$dev"/sound/card* 2>/dev/null | grep -oE '[0-9]+$' | head -1)
                        [ -n "$c" ] && holders /dev/snd/*C"$c"*
                        echo "   pipewire/wireplumber? In YOUR (non-sudo) terminal: systemctl --user stop wireplumber pipewire pipewire-pulse"
                        echo "   and start them again after the run." ;;
                    *) echo "   (try: sudo fuser -v /dev/* 2>&1 | grep -v '^$')" ;;
                esac
                echo "   Waiting: the unbind finishes on its own once the holder exits (kill it from another terminal, or Ctrl-C)."
            fi
        done
        wait "$w" || { echo "!! unbind of $dev from $drv failed"; return 1; }
    fi
    echo vfio-pci > "/sys/bus/pci/devices/$dev/driver_override"
    echo "$dev" > /sys/bus/pci/drivers_probe
    [ "$(cur_driver "$dev")" = vfio-pci ] || { echo "!! $dev did not bind to vfio-pci"; return 1; }
    echo "   $dev -> vfio-pci"
}

to_host() {
    local dev=$1
    [ "$(cur_driver "$dev")" = vfio-pci ] && echo "$dev" > /sys/bus/pci/drivers/vfio-pci/unbind
    echo "" > "/sys/bus/pci/devices/$dev/driver_override"
    echo "$dev" > /sys/bus/pci/drivers_probe
    echo "   $dev -> $(cur_driver "$dev")"
}

echo "== binary: $($FC --version | head -1)"
chown -R "${SUDO_USER:-user}:${SUDO_USER:-user}" build/cargo_target 2>/dev/null || true
RC=0

if [ "$WHAT" = gpu ] || [ "$WHAT" = all ]; then
    LOG=build/hw-gpu-$(date +%Y%m%d-%H%M%S).log
    echo "== GPU round -> $LOG"
    # scratch copy so the proven June rootfs is never written to
    [ -f build/hw-rootfs-gpu.ext4 ] || { echo "   making scratch rootfs copy ..."; cp --reflink=auto "$GPU_ROOTFS" build/hw-rootfs-gpu.ext4; }
    # KILL_GPU_HOLDERS=1 (opt-in): kill whatever holds the GPU open (usually a browser's 'RDD Process'
    # video decoder, which the browser just respawns) instead of refusing to start.
    if [ "${KILL_GPU_HOLDERS:-0}" = 1 ] && ! holders /dev/nvidia0 /dev/nvidiactl /dev/nvidia-uvm /dev/nvidia-modeset >/dev/null; then
        pids=$(fuser /dev/nvidia0 /dev/nvidiactl /dev/nvidia-uvm 2>/dev/null | tr -s ' \n' ' ')
        echo "   KILL_GPU_HOLDERS=1: killing GPU holder(s):$pids"; kill $pids 2>/dev/null; sleep 2
    fi
    if ! holders /dev/nvidia0 /dev/nvidiactl /dev/nvidia-uvm /dev/nvidia-modeset; then
        echo "!! something still has the GPU open (above) - the nvidia unbind would hang until it exits."
        echo "   Close it (a browser's video decoder / 'RDD Process' is the usual culprit: kill <PID>),"
        echo "   or rerun with KILL_GPU_HOLDERS=1 to have this script kill it."
        RC=1
    elif to_vfio "$GPU" && { [ -z "$GPU_AUDIO" ] || to_vfio "$GPU_AUDIO"; }; then
        python3 tools/test_vfio_m3_proof.py stability --runs 3 \
            --bdf "$GPU" --kernel "$GPU_KERNEL" \
            --rootfs build/hw-rootfs-gpu.ext4 --firecracker "$FC" \
            --mem-mib 4096 --vcpus 2 2>&1 | tee "$LOG"
        R=${PIPESTATUS[0]}; [ "$R" = 0 ] || RC=1
        echo "== GPU harness exit: $R   (NVRM lines seen in serial: $(grep -c NVRM "$LOG"))"
    else RC=1; fi
    echo "== restoring GPU to host drivers"; [ -n "$GPU_AUDIO" ] && to_host "$GPU_AUDIO"; to_host "$GPU"
fi

if [ "$WHAT" = nic ] || [ "$WHAT" = all ]; then
    LOG=build/hw-nic-$(date +%Y%m%d-%H%M%S).log
    echo "== NIC round -> $LOG"
    [ -f build/hw-rootfs-nic.ext4 ] || cp --reflink=auto "$NIC_ROOTFS" build/hw-rootfs-nic.ext4
    if to_vfio "$NIC"; then
        # vmlinux-host + rootfs.ext4 carry the 6.17.9 modules incl. r8169.ko (the M1 combo).
        # No pciutils in this rootfs, so judge by the serial log: look for the r8169 probe line.
        # The M1 harness watches the serial console forever (deadline patched out for interactive use),
        # so stop it with SIGINT after 75s: that runs its own report + Firecracker cleanup. -u keeps
        # Python's stdout unbuffered so the serial log survives in $LOG.
        rm -f /tmp/fc_vfio_test.log
        # --foreground + stdin from /dev/null: plain `timeout` puts the child in a background process
        # group, and Firecracker's tcsetattr(stdin) at vCPU start then gets SIGTTOU -> the whole VMM is
        # silently STOPPED (threads in do_signal_stop). That was the "hang" in the first NIC rounds.
        # rootfs.ext4's module index only covers a staged subset, so udev never auto-loads r8169 even
        # though the .ko is present (and has no deps). Have systemd insmod it at boot and print the link.
        R8169_KO=/lib/modules/$NIC_GUEST_KVER/kernel/drivers/net/ethernet/realtek/r8169.ko
        # (dmesg because Ubuntu's sysctl sets kernel.printk=4 during boot, hiding the INFO-level probe line from the console)
        # run_*_action=none: systemd.run's default success action is to POWER OFF the machine once the command
        # finishes, which would halt the guest before the login prompt.
        # systemd.run= also makes kernel-command-line.target the boot target (no getty), so the hook
        # hands control back to the normal boot at the end -> the login prompt still appears.
        PROBE="systemd.run=\"/bin/sh -c 'insmod $R8169_KO >/dev/ttyS0 2>&1; sleep 2; dmesg | grep -i r8169 >/dev/ttyS0; ip -br link >/dev/ttyS0; systemctl --no-block start multi-user.target'\" systemd.run_success_action=none systemd.run_failure_action=none"
        timeout --foreground -s INT -k 15 75 python3 -u tools/test_vfio_m1.py --bdf "$NIC" --kernel "$NIC_KERNEL" \
            --rootfs build/hw-rootfs-nic.ext4 --firecracker "$FC" --vcpus 1 --boot-args "$PROBE" < /dev/null 2>&1 | tee "$LOG" &
        TP=$!
        for t in $(seq 1 90); do
            sleep 1; kill -0 "$TP" 2>/dev/null || break
            # If InstanceStart has not completed after 25s, dump every firecracker thread's kernel stack.
            if [ "$t" = 25 ] && ! grep -q 'event_end: build microvm for boot' /tmp/fc_vfio_test.log 2>/dev/null; then
                FCPID=$(pgrep -f "^$FC " | head -1)
                { echo; echo "== InstanceStart still not finished after 25s: firecracker pid ${FCPID:-?} thread dump"
                  for task in /proc/${FCPID:-0}/task/*; do
                      [ -d "$task" ] || continue
                      echo "--- $(cat "$task/comm") wchan=$(cat "$task/wchan")"; head -14 "$task/stack" 2>/dev/null
                  done
                  echo "--- fc log tail:"; tail -3 /tmp/fc_vfio_test.log 2>/dev/null | cut -c1-160; echo; } | tee -a "$LOG"
            fi
        done
        wait "$TP" 2>/dev/null
        echo "== guest reached login: $(grep -aqE 'login:' "$LOG" && echo yes || echo no)"
        echo "== NIC enumerated by guest (10ec:8168 as class 0x0200 Ethernet): $(grep -aqE 'pci 0000:00:0[0-9]\.0: \[10ec:8168\]' "$LOG" && echo yes || echo NO)"
        echo "== guest link table (from ip -br link):"; grep -aE '^(eth|enp|lo)[0-9a-z]*\s' "$LOG" | head -4 | sed 's/^/   /'
        echo "== r8169 probe lines (guest dmesg):"; grep -aE 'r8169 0000:00:.*(RTL|XID|MSI)' "$LOG" | head -3 | sed 's/^/   /' || true
        # PASS = the guest driver bound the passed-through NIC: an eth* interface carrying the RTL8111's hardware MAC.
        if grep -aqE "^eth[0-9]+\s+\S+\s+$NIC_MAC" "$LOG"; then echo "== r8169 bound in guest (eth0 has the RTL8111 hardware MAC): yes"
        else echo "== r8169 bound in guest: NO"; RC=1; fi
    else RC=1; fi
    echo "== restoring NIC to host driver"; to_host "$NIC"
fi

echo; echo "== hardware round result: $([ $RC = 0 ] && echo PASS || echo FAIL)"; exit $RC
