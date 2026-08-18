#!/usr/bin/env bash
# Build the module + initramfs, boot them under QEMU, and turn the serial log
# into an exit code. Drivable non-interactively:
#
#   ssh azure 'cd ~/alloyfs/kernel/test && ./run.sh --stage 1'
#
# Exit 0 ONLY when the guest reached ALLOYFS-DONE with zero ALLOYFS-FAIL lines and no
# kernel splat. A panic/oops/WARN kills QEMU immediately (panic=-1 oops=panic
# panic_on_warn=1), so a crashed guest is a fast failure, never a hang.
#
# The guest runs under KVM wherever the host offers it and falls back to TCG
# where it does not, so the same command is simply faster on a host with
# virtualisation than on one without. ALLOYFS_QEMU_ACCEL forces either.
#
# What a host has to provide: qemu-system-x86_64, gcc, make, cpio,
# busybox-static, and a kernel that can do BOTH jobs — supply a bootable image
# and build an out-of-tree module. Those are one kernel, not two, because a
# module only loads into the kernel whose build tree compiled it.
#
# On a normal distro host the running kernel is that kernel and nothing needs
# saying. WSL2 is the case where it is not: its Microsoft kernel ships neither
# /boot/vmlinuz-* nor /lib/modules/$(uname -r)/build, and no headers for it
# exist in the Ubuntu archive. Installing linux-image-virtual there does not
# help either — the archive kernel is far newer than the module targets, and
# the VFS moves (Ubuntu 26.04 ships 7.0, where i_state became a struct, ->mkdir
# returns a dentry and generic_delete_inode is gone). So on WSL2 the harness
# needs a kernel of its own:
#
#   ./build-debug-kernel.sh                          # ~/kbuild/linux-6.14.11
#   export ALLOYFS_DEBUG_KERNEL=~/kbuild/linux-6.14.11
#   ./run.sh --stage 1
#
# which is no loss: the tree that build-debug-kernel.sh produces is the one the
# expensive stages want anyway. Measured on that tree, whole suite, guest time:
# 156s under KVM against 367s under TCG on the same laptop.
set -euo pipefail
cd "$(dirname "$0")"

STAGE=0
TIMEOUT=180
MACHINE="${ALLOYFS_QEMU_MACHINE:-microvm}"
MEM="${ALLOYFS_QEMU_MEM:-256}"
SMP="${ALLOYFS_QEMU_SMP:-1}"
KVER="$(uname -r)"
KVER_SET=0
BUILD=1
# A host with no usable distro kernel (see the resolution block below) can name
# a built source tree once in the environment instead of on every command line.
DEBUG_KDIR="${ALLOYFS_DEBUG_KERNEL:-}"
# lockdep reports one splat per lock-order violation and then stays quiet, so
# panic_on_warn would hide everything after the first. The debug kernel keeps
# oops=panic (a real bug still stops the run) but lets WARNs accumulate.
PANIC_ON_WARN=1

TIMEOUT_SET=0

while [ $# -gt 0 ]; do
	case "$1" in
	--stage)   STAGE="$2"; shift 2 ;;
	--timeout) TIMEOUT="$2"; TIMEOUT_SET=1; shift 2 ;;
	--machine) MACHINE="$2"; shift 2 ;;
	--kver)    KVER="$2"; KVER_SET=1; shift 2 ;;
	# Point at a built kernel source tree (see build-debug-kernel.sh): use
	# ITS bzImage and build the module against ITS headers, so the checkers
	# compiled into that kernel actually apply to our code.
	--debug-kernel) DEBUG_KDIR="$2"; shift 2 ;;
	--no-build) BUILD=0; shift ;;
	*) echo "unknown option: $1" >&2; exit 2 ;;
	esac
done

OUT="out"
mkdir -p "$OUT"

# Stage 6 replaces the C test daemon with the real Rust client, which has to be
# built and carried into the guest. The image lives in guest RAM and the client
# is an order of magnitude bigger than the C probes, so the box needs more of
# it — and a release build plus a real agent handshake needs more wall clock.
if [ "$STAGE" -ge 6 ]; then
	if [ -z "${ALLOYFS_BIN:-}" ]; then
		CARGO="${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}"
		[ -x "$CARGO" ] || { echo "cargo not found (set CARGO= or ALLOYFS_BIN=)" >&2; exit 1; }
		echo "==> building the alloyfs client (release)"
		( cd ../.. && "$CARGO" build --release --bin alloyfs ) >/dev/null
		ALLOYFS_BIN="$(cd ../.. && pwd)/target/release/alloyfs"
	fi
	[ -x "$ALLOYFS_BIN" ] || { echo "no alloyfs binary at $ALLOYFS_BIN" >&2; exit 1; }
	export ALLOYFS_BIN
	# ~13 MB of image (client + glibc + busybox) plus two tokio processes.
	# Modest on purpose: the build host has under a gigabyte itself.
	MEM="${ALLOYFS_QEMU_MEM:-384}"
	[ "$TIMEOUT_SET" -eq 1 ] || TIMEOUT=300
fi

# One kernel serves two roles at once: the guest boots its image, and the module
# is compiled against its headers. They cannot be separated — an out-of-tree
# module only loads into the kernel whose build tree produced it.
#
# The running kernel is the obvious candidate, and on a plain distro host it is
# the only one. It is not always usable, though. WSL2 runs a Microsoft kernel
# that ships neither a /boot image nor a build tree, so there is nothing to boot
# and nothing to compile against; a host that keeps several kernels installed
# may also be running an older one than the one whose headers are present.
# Prefer the running kernel when it can do both jobs, otherwise take the newest
# installed kernel that can.
kver_usable() { [ -d "/lib/modules/$1/build" ] && [ -e "/boot/vmlinuz-$1" ]; }

if [ -z "$DEBUG_KDIR" ] && [ "$KVER_SET" -eq 0 ] && ! kver_usable "$KVER"; then
	for cand in $(ls -1 /lib/modules 2>/dev/null | sort -rV); do
		kver_usable "$cand" || continue
		echo "==> running kernel $KVER has no build tree or image; using $cand"
		KVER="$cand"
		break
	done
fi

KDIR="/lib/modules/$KVER/build"
KERNEL="/boot/vmlinuz-$KVER"

if [ -n "$DEBUG_KDIR" ]; then
	KERNEL="$DEBUG_KDIR/arch/x86/boot/bzImage"
	KDIR="$DEBUG_KDIR"
	PANIC_ON_WARN=0
	[ -r "$KERNEL" ] || { echo "no bzImage in $DEBUG_KDIR (build it first)" >&2; exit 1; }
	echo "==> debug kernel: $KERNEL"
fi

# Ubuntu ships /boot/vmlinuz-* as 0600 root. Stage one readable copy (cached
# across runs) rather than loosening permissions on the host's boot image.
if [ ! -r "$KERNEL" ]; then
	STAGED="$OUT/vmlinuz-$KVER"
	if [ ! -r "$STAGED" ]; then
		[ -e "$KERNEL" ] || {
			echo "no kernel image at $KERNEL" >&2
			echo "  nothing installed on this host can host the module. Either install" >&2
			echo "  a distro kernel (apt install linux-image-virtual linux-headers-generic)" >&2
			echo "  or point the harness at a built source tree with --debug-kernel DIR" >&2
			echo "  / ALLOYFS_DEBUG_KERNEL=DIR (see build-debug-kernel.sh)." >&2
			exit 1
		}
		echo "==> staging a readable copy of $KERNEL (needs sudo, once)"
		# -n, because a host whose sudo wants a password would otherwise block
		# on a prompt no non-interactive caller can answer — and this harness is
		# driven over ssh and from CI. Fail with the command to run by hand.
		if ! { sudo -n cp "$KERNEL" "$STAGED" &&
		       sudo -n chown "$(id -u):$(id -g)" "$STAGED"; }; then
			rm -f "$STAGED"
			echo "sudo could not stage the image. Run this once, by hand:" >&2
			echo "  sudo install -m 0644 -o $(id -un) -g $(id -gn) $KERNEL $PWD/$STAGED" >&2
			exit 1
		fi
	fi
	KERNEL="$STAGED"
fi

[ -d "$KDIR" ] || { echo "no kernel build tree at $KDIR" >&2; exit 1; }

# Stage 0 is the harness self-test: no module yet.
KO=""
if [ "$STAGE" -ge 1 ] && [ "$BUILD" -eq 1 ]; then
	echo "==> building module against ${DEBUG_KDIR:-$KVER}"
	make -C ../alloyfs KDIR="$KDIR" >/dev/null
	KO="../alloyfs/alloyfs.ko"
	[ -f "$KO" ] || { echo "module build produced no alloyfs.ko" >&2; exit 1; }
fi

echo "==> building initramfs (stage $STAGE)"
./mkinitramfs.sh "$STAGE" "$OUT" $KO

SERIAL="$OUT/serial.log"
rm -f "$SERIAL"

# TCG — QEMU's default, translating every guest instruction in software — is the
# only option on a host without virtualisation, and it is what the harness ran
# under for its whole life. Where /dev/kvm is present and writable the guest can
# execute on the real CPU instead. That is worth roughly 2x on a bare boot and
# considerably more on the debug kernel, whose lockdep and DEBUG_OBJECTS
# instrumentation turns every lock and every object lifecycle into extra
# instructions for TCG to translate.
#
# Detected rather than assumed, so one harness covers both: the remote build box
# has no /dev/kvm, WSL2 with nested virtualisation does. No kvm:tcg fallback
# chain, because the CPU model has to match the choice and QEMU rejects
# -cpu host under TCG; a wrong detection should fail loudly, not silently
# degrade to a run ten times slower than expected.
ACCEL="${ALLOYFS_QEMU_ACCEL:-}"
if [ -z "$ACCEL" ]; then
	if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then ACCEL=kvm; else ACCEL=tcg; fi
fi
# qemu64 is a deliberately minimal model, which is what TCG wants. Under KVM
# there is no translator to keep simple, and passing the host CPU through avoids
# trapping on features the guest would otherwise find missing.
case "$ACCEL" in
kvm) CPU="${ALLOYFS_QEMU_CPU:-host}" ;;
*)   CPU="${ALLOYFS_QEMU_CPU:-qemu64}" ;;
esac

# -no-reboot + panic=-1: any panic terminates QEMU instead of looping.
# -serial file: (not mon:stdio) so this works over a non-interactive ssh.
QEMU_ARGS=(
	-accel "$ACCEL" -cpu "$CPU" -smp "$SMP" -m "$MEM"
	-no-reboot -nodefaults -no-user-config -display none
	-kernel "$KERNEL"
	-initrd "$OUT/initramfs.cpio"
	# reboot=triple: microvm has neither i8042 nor ACPI, so the kernel's usual
	# reset methods dead-end and the guest hangs after its tests. A triple
	# fault is always visible to QEMU, which exits because of -no-reboot.
	-append "console=ttyS0 earlyprintk=serial,ttyS0 rdinit=/init nokaslr panic=-1 oops=panic panic_on_warn=$PANIC_ON_WARN reboot=triple loglevel=7 tsc=unstable no_timer_check${DEBUG_KDIR:+ slub_debug=FZPU}"
	-serial "file:$SERIAL"
)
case "$MACHINE" in
microvm) QEMU_ARGS=(-M microvm,acpi=off,rtc=off "${QEMU_ARGS[@]}") ;;
*)       QEMU_ARGS=(-M "$MACHINE" "${QEMU_ARGS[@]}") ;;
esac

echo "==> booting ($MACHINE, $ACCEL/$CPU, ${MEM}M, timeout ${TIMEOUT}s)"
started=$(date +%s)
set +e
timeout "$TIMEOUT" qemu-system-x86_64 "${QEMU_ARGS[@]}"
qemu_rc=$?
set -e
elapsed=$(( $(date +%s) - started ))
echo "==> guest exited rc=$qemu_rc after ${elapsed}s"

[ -s "$SERIAL" ] || { echo "FAIL: empty serial log — the guest produced nothing" >&2; exit 1; }

# rc=124 is `timeout` killing a wedged guest. Even if the sentinels made it
# out, a guest that won't exit is a harness failure worth surfacing.
if [ "$qemu_rc" -eq 124 ]; then
	echo "FAIL: guest did not exit within ${TIMEOUT}s (hung after its tests?). Tail:" >&2
	tail -20 "$SERIAL" >&2
	exit 1
fi

# A kernel splat is a failure even if the sentinels somehow made it out.
if grep -nE 'Kernel panic|BUG:|WARNING:|general protection fault|unable to handle page fault|KASAN:|INFO: possible circular locking|stack segment' "$SERIAL" >/dev/null; then
	echo "FAIL: kernel splat in serial log:" >&2
	grep -nE -B5 -A55 'Kernel panic|BUG:|WARNING:|general protection fault|unable to handle page fault|KASAN:|INFO: possible circular locking' "$SERIAL" | head -80 >&2
	exit 1
fi

if ! grep -q '^ALLOYFS-DONE' "$SERIAL"; then
	echo "FAIL: guest never reached ALLOYFS-DONE (timeout or early exit). Tail:" >&2
	tail -40 "$SERIAL" >&2
	exit 1
fi

if grep -q '^ALLOYFS-FAIL:' "$SERIAL"; then
	echo "FAIL: failing cases:" >&2
	grep -n -B30 '^ALLOYFS-FAIL:' "$SERIAL" | tail -60 >&2
	exit 1
fi

passed=$(grep -c '^ALLOYFS-PASS:' "$SERIAL" || true)
echo "PASS: stage $STAGE — $passed case(s), ${elapsed}s, log: $SERIAL"
