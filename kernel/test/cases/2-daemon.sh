# Stage 2: the filesystem served by a real daemon over /dev/alloyfs. Every
# name, size and byte in this case crossed the kernel<->userspace ABI to get
# here: lookup, getattr, readdir and read.
. /tests/lib.sh

MNT=/mnt/alloyfs

insmod /lib/modules/alloyfs.ko || { echo "  FAIL: insmod"; exit 1; }
check "device created" test -c /dev/alloyfs
mkdir -p $MNT

# The kernel resolves fd= in the MOUNTING process's descriptor table, so the
# daemon and mount(8) must share one open file description: this shell opens
# the device, the daemon inherits it, and mount names the same number.
exec 3<> /dev/alloyfs || { echo "  FAIL: cannot open /dev/alloyfs"; exit 1; }
ok "opened /dev/alloyfs on fd 3"

# Serve that same connection from a background process that inherits fd 3.
alloyd --fd 3 > /tmp/alloyd.log 2>&1 &
ALLOYD_PID=$!
sleep 0.3

mount -t alloyfs -o fd=3 none $MNT
if [ $? -ne 0 ]; then
	echo "  FAIL: mount -o fd=3"
	cat /tmp/alloyd.log
	exit 1
fi
ok "mounted with fd=3"

# --- the tree comes entirely from the daemon --------------------------------
eq "root listing" "dir
one.txt" "$(ls $MNT | sort)"
check "subdir listed"   test -d $MNT/dir
eq "file contents"      "first file" "$(cat $MNT/one.txt)"
eq "nested contents"    "second file, in a subdirectory" "$(cat $MNT/dir/two.txt)"
eq "size from getattr"  "10" "$(stat -c %s $MNT/one.txt)"
check "missing name is ENOENT" sh -c "! cat $MNT/nope.txt 2>/dev/null"

# --- daemon death must fail operations, not wedge them ----------------------
kill -9 $ALLOYD_PID 2>/dev/null
exec 3>&-      # drop our copy so the connection really closes
sleep 0.3
check "reads fail after daemon death" sh -c "! cat $MNT/one.txt 2>/dev/null"
ok "no hang after daemon death"

umount $MNT
check "unmounted" sh -c '! grep -q " alloyfs " /proc/mounts'
rmmod alloyfs
check "rmmod clean" sh -c '! lsmod | grep -q alloyfs'

summary
