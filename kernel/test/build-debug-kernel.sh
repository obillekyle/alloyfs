#!/usr/bin/env bash
# Build a kernel with the runtime checkers turned on, for stage 5.
#
# Distro kernels ship with lockdep OFF — it is far too expensive for
# production — so the bug classes that matter most for a filesystem module
# (lock inversion, sleeping in atomic context, use-after-free) are invisible
# on the kernel we have been testing against. This builds one that sees them.
#
# Deliberately NOT KASAN: on a 2-core box with 843 MB of RAM the compile cost
# is prohibitive, and lockdep + DEBUG_OBJECTS + SLUB_DEBUG poisoning (boot
# with slub_debug=FZPU) covers most of the same ground for a fraction of the
# build. KASAN is a later option if a memory bug proves elusive.
#
# Usage: ./build-debug-kernel.sh [version]   (default below)
set -euo pipefail

VER="${1:-6.14.11}"
SERIES="v6.x"
ROOT="${DS_KBUILD_ROOT:-$HOME/kbuild}"
SRC="$ROOT/linux-$VER"
JOBS="${DS_KBUILD_JOBS:-$(nproc)}"

mkdir -p "$ROOT"
cd "$ROOT"

# Build dependencies (idempotent).
need=""
for p in flex bison libssl-dev libelf-dev bc; do
	dpkg -s "$p" >/dev/null 2>&1 || need="$need $p"
done
if [ -n "$need" ]; then
	echo "==> installing build deps:$need"
	sudo apt-get update -qq
	sudo apt-get install -y -qq $need
fi

# 843 MB of RAM is not enough headroom for two gcc processes on the big
# translation units; a swapfile turns a possible OOM into merely slower.
if [ "$(swapon --show --noheadings | wc -l)" -eq 0 ]; then
	echo "==> adding a 2G swapfile (build headroom)"
	sudo fallocate -l 2G /swapfile
	sudo chmod 600 /swapfile
	sudo mkswap -q /swapfile
	sudo swapon /swapfile
fi

if [ ! -d "$SRC" ]; then
	TARBALL="linux-$VER.tar.xz"
	[ -f "$TARBALL" ] || {
		echo "==> downloading linux $VER"
		curl -fL --progress-bar -o "$TARBALL" \
			"https://cdn.kernel.org/pub/linux/kernel/$SERIES/$TARBALL"
	}
	echo "==> extracting"
	tar xf "$TARBALL"
fi

cd "$SRC"

if [ ! -f .config ]; then
	echo "==> configuring (defconfig + checkers)"
	make defconfig >/dev/null

	# The checkers this kernel exists for.
	scripts/config \
		--enable DEBUG_KERNEL \
		--enable PROVE_LOCKING \
		--enable DEBUG_ATOMIC_SLEEP \
		--enable DEBUG_SPINLOCK \
		--enable DEBUG_MUTEXES \
		--enable DEBUG_RWSEMS \
		--enable DEBUG_LIST \
		--enable DEBUG_OBJECTS \
		--enable DEBUG_OBJECTS_FREE \
		--enable DEBUG_OBJECTS_TIMERS \
		--enable DEBUG_VM \
		--enable SLUB_DEBUG \
		--enable DEBUG_KMEMLEAK \
		--enable STACKTRACE \
		--enable FRAME_POINTER \
		--enable DEBUG_INFO_NONE

	# Things the harness itself needs (all in defconfig, asserted anyway so
	# a future defconfig change cannot silently break the tests).
	scripts/config \
		--enable MODULES \
		--enable MODULE_UNLOAD \
		--enable INOTIFY_USER \
		--enable TMPFS \
		--enable PROC_FS \
		--enable SYSFS \
		--enable BLK_DEV_INITRD \
		--enable BINFMT_ELF \
		--enable SERIAL_8250 \
		--enable SERIAL_8250_CONSOLE \
		--disable MODULE_SIG \
		--disable SECURITY_LOCKDOWN_LSM

	make olddefconfig >/dev/null
fi

# `modules` (not just modules_prepare) is required: it is the modpost pass over
# the built kernel that writes Module.symvers, and without that file an
# out-of-tree build resolves NOTHING — every symbol down to _printk comes back
# undefined. defconfig only builds a handful of modules, so the extra cost is
# small next to the vmlinux link.
echo "==> building bzImage + modules with -j$JOBS (this is the long part)"
make -j"$JOBS" bzImage modules
[ -f Module.symvers ] || { echo "no Module.symvers: out-of-tree builds will fail" >&2; exit 1; }

echo
echo "kernel:  $SRC/arch/x86/boot/bzImage"
echo "kdir:    $SRC"
echo "run it:  ./run.sh --stage 5 --debug-kernel $SRC"
