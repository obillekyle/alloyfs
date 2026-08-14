#!/usr/bin/env bash
# Build the module + initramfs, boot them under QEMU, and turn the serial log
# into an exit code. Drivable non-interactively:
#
#   ssh azure 'cd ~/drive-sync/kernel/test && ./run.sh --stage 1'
#
# Exit 0 ONLY when the guest reached DS-DONE with zero DS-FAIL lines and no
# kernel splat. A panic/oops/WARN kills QEMU immediately (panic=-1 oops=panic
# panic_on_warn=1), so a crashed guest is a fast failure, never a hang.
set -euo pipefail
cd "$(dirname "$0")"

STAGE=0
TIMEOUT=180
MACHINE="${DS_QEMU_MACHINE:-microvm}"
MEM="${DS_QEMU_MEM:-256}"
SMP="${DS_QEMU_SMP:-1}"
KVER="$(uname -r)"
BUILD=1
DEBUG_KDIR=""
# lockdep reports one splat per lock-order violation and then stays quiet, so
# panic_on_warn would hide everything after the first. The debug kernel keeps
# oops=panic (a real bug still stops the run) but lets WARNs accumulate.
PANIC_ON_WARN=1

while [ $# -gt 0 ]; do
	case "$1" in
	--stage)   STAGE="$2"; shift 2 ;;
	--timeout) TIMEOUT="$2"; shift 2 ;;
	--machine) MACHINE="$2"; shift 2 ;;
	--kver)    KVER="$2"; shift 2 ;;
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

# Ubuntu ships /boot/vmlinuz-* as 0600 root. Stage one readable copy (cached
# across runs) rather than loosening permissions on the host's boot image.
KDIR="/lib/modules/$KVER/build"
KERNEL="/boot/vmlinuz-$KVER"

if [ -n "$DEBUG_KDIR" ]; then
	KERNEL="$DEBUG_KDIR/arch/x86/boot/bzImage"
	KDIR="$DEBUG_KDIR"
	PANIC_ON_WARN=0
	[ -r "$KERNEL" ] || { echo "no bzImage in $DEBUG_KDIR (build it first)" >&2; exit 1; }
	echo "==> debug kernel: $KERNEL"
fi

if [ ! -r "$KERNEL" ]; then
	STAGED="$OUT/vmlinuz-$KVER"
	if [ ! -r "$STAGED" ]; then
		[ -e "$KERNEL" ] || { echo "no kernel image at $KERNEL" >&2; exit 1; }
		echo "==> staging a readable copy of $KERNEL (needs sudo, once)"
		sudo cp "$KERNEL" "$STAGED"
		sudo chown "$(id -u):$(id -g)" "$STAGED"
	fi
	KERNEL="$STAGED"
fi

# Stage 0 is the harness self-test: no module yet.
KO=""
if [ "$STAGE" -ge 1 ] && [ "$BUILD" -eq 1 ]; then
	echo "==> building module against ${DEBUG_KDIR:-$KVER}"
	make -C ../ds-fs KDIR="$KDIR" >/dev/null
	KO="../ds-fs/ds_fs.ko"
	[ -f "$KO" ] || { echo "module build produced no ds_fs.ko" >&2; exit 1; }
fi

echo "==> building initramfs (stage $STAGE)"
./mkinitramfs.sh "$STAGE" "$OUT" $KO

SERIAL="$OUT/serial.log"
rm -f "$SERIAL"

# -no-reboot + panic=-1: any panic terminates QEMU instead of looping.
# -serial file: (not mon:stdio) so this works over a non-interactive ssh.
QEMU_ARGS=(
	-cpu qemu64 -smp "$SMP" -m "$MEM"
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

echo "==> booting ($MACHINE, ${MEM}M, timeout ${TIMEOUT}s)"
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

if ! grep -q '^DS-DONE' "$SERIAL"; then
	echo "FAIL: guest never reached DS-DONE (timeout or early exit). Tail:" >&2
	tail -40 "$SERIAL" >&2
	exit 1
fi

if grep -q '^DS-FAIL:' "$SERIAL"; then
	echo "FAIL: failing cases:" >&2
	grep -n -B30 '^DS-FAIL:' "$SERIAL" | tail -60 >&2
	exit 1
fi

passed=$(grep -c '^DS-PASS:' "$SERIAL" || true)
echo "PASS: stage $STAGE — $passed case(s), ${elapsed}s, log: $SERIAL"
