#!/usr/bin/env bash
# Build the test initramfs: busybox-static + our probes + the module + the
# per-stage test cases. Uncompressed newc cpio — the kernel accepts it as-is
# and skipping gzip saves a second of every iteration.
#
#   ./mkinitramfs.sh <stage> <out-dir> [module.ko]
set -euo pipefail
cd "$(dirname "$0")"

STAGE="${1:?usage: mkinitramfs.sh <stage> <out-dir> [module.ko]}"
OUT="${2:?}"
KO="${3:-}"

BUSYBOX="${BUSYBOX:-/usr/bin/busybox}"
[ -x "$BUSYBOX" ] || { echo "busybox-static not found at $BUSYBOX" >&2; exit 1; }

ROOT="$OUT/root"
rm -rf "$ROOT"
mkdir -p "$ROOT"/{bin,dev,proc,sys,tmp,tests,mnt,lib/modules}

cp "$BUSYBOX" "$ROOT/bin/busybox"
# Applets the harness uses. busybox resolves these via argv[0].
for a in sh mount umount mkdir mkfifo rmdir insmod rmmod lsmod dmesg cat ls sleep \
         kill poweroff reboot sync grep sort md5sum dd echo printf test true \
         false touch rm mv cp stat find head tail wc env date; do
	ln -sf busybox "$ROOT/bin/$a"
done

# Our probes, statically linked so the initramfs needs no libc.
gcc -static -Os -Wall -Wextra -o "$ROOT/bin/ds-inotify" tools/ds-inotify.c
gcc -static -Os -Wall -Wextra -o "$ROOT/bin/dsd" tools/dsd.c
gcc -static -Os -Wall -Wextra -pthread -o "$ROOT/bin/ds-devtest" tools/ds-devtest.c
strip "$ROOT/bin/ds-inotify" "$ROOT/bin/dsd" "$ROOT/bin/ds-devtest" 2>/dev/null || true

[ -n "$KO" ] && cp "$KO" "$ROOT/lib/modules/"

cp lib.sh "$ROOT/tests/lib.sh"
# Stage N runs cases N-*.sh. Stage 0 has none: it just proves the boot works.
for f in cases/"$STAGE"-*.sh; do
	[ -e "$f" ] || continue
	cp "$f" "$ROOT/tests/"
done

cat > "$ROOT/init" <<EOF
#!/bin/sh
# PID 1 in the guest. Prints sentinels run.sh greps for; never returns.
export PATH=/bin
mount -t proc     proc /proc
mount -t sysfs    sys  /sys
mount -t devtmpfs dev  /dev
mount -t tmpfs    tmp  /tmp   # scratch for test logs, and the tmpfs the
                              # differential assertions compare us against
echo "DS-BOOT-OK"
echo "DS-STAGE: $STAGE"

fail=0
for t in /tests/$STAGE-*.sh; do
	[ -e "\$t" ] || continue
	echo "DS-CASE: \$t"
	if sh "\$t"; then
		echo "DS-PASS: \$t"
	else
		echo "DS-FAIL: \$t"
		fail=1
	fi
done
echo "DS-FAILED: \$fail"
echo "DS-DONE"
# microvm has no ACPI, so poweroff cannot signal QEMU. A guest RESET does:
# QEMU runs with -no-reboot, so a reboot terminates it immediately.
sync
reboot -f
sleep 5
poweroff -f          # fallback for ACPI machine types
EOF
chmod +x "$ROOT/init"

# Deterministic archive: sorted order, fixed ownership.
( cd "$ROOT" && find . -print0 | LC_ALL=C sort -z |
	cpio --null -o -H newc --owner=0:0 --quiet ) > "$OUT/initramfs.cpio"

echo "initramfs: $(du -h "$OUT/initramfs.cpio" | cut -f1) ($(find "$ROOT" | wc -l) entries)"
