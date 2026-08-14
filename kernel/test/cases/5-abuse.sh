# Stage 5: hostility and concurrency.
#
# A filesystem lives at the mercy of its daemon, and the daemon is ordinary
# userspace: it can die mid-request, stop answering, or send nonsense. None
# of that may take the kernel with it, and — just as important — none of it
# may leave a process stuck in D state, because an unkillable process is a
# reboot.
#
# Run this against the debug kernel (./run.sh --stage 5 --debug-kernel DIR)
# so lockdep and the object/slab checkers are watching. The final check
# scans dmesg for anything they had to say.
. /tests/lib.sh

MNT=/mnt/dsfs
CTL=/tmp/dsd.ctl

insmod /lib/modules/ds_fs.ko || { echo "  FAIL: insmod"; exit 1; }
mkdir -p $MNT
mkfifo $CTL 2>/dev/null

start() {  # start [dsd args...] — fresh connection + mount
	exec 3<> /dev/ds-fs || return 1
	dsd --fd 3 --ctl $CTL "$@" > /tmp/dsd.log 2>&1 &
	DSD_PID=$!
	sleep 0.3
	mount -t dsfs -o fd=3 none $MNT
}
stop() {
	kill -9 $DSD_PID 2>/dev/null
	exec 3>&-
	sleep 0.2
	umount $MNT 2>/dev/null
	sleep 0.1
}

# --- 1. a daemon that stops answering must not create unkillable processes --
start --hang-on lookup || { echo "  FAIL: mount"; exit 1; }
ok "mounted against a daemon that will stop answering"

# LOOKUP never gets answered, so this cat blocks in the filesystem.
cat $MNT/one.txt > /dev/null 2>&1 &
STUCK=$!
sleep 0.5
check "victim is running" kill -0 $STUCK

# The request sleeps killable: SIGKILL must actually land. If this hangs,
# the box needs a reboot — which is exactly the bug being tested for.
kill -9 $STUCK 2>/dev/null
n=0
while kill -0 $STUCK 2>/dev/null; do
	n=$((n + 1))
	[ $n -gt 100 ] && break
	sleep 0.05
done
if kill -0 $STUCK 2>/dev/null; then
	fail "SIGKILL frees a process waiting on a silent daemon"
else
	ok "SIGKILL frees a process waiting on a silent daemon"
fi
wait $STUCK 2>/dev/null
stop

echo "  .. section 2"
# --- 2. corrupt framing is rejected, not believed --------------------------
# Corruption starts after the mount's own root GETATTR, so we get a live
# mount served by a daemon that then sends len=0xffffffff on everything.
# The kernel must REFUSE those frames (the daemon's write() gets an error)
# rather than trusting the length — and the blocked reader stays killable.
start --corrupt-after 1 || { echo "  FAIL: mount"; exit 1; }
cat $MNT/one.txt > /dev/null 2>&1 &
STUCK=$!
sleep 0.6
check "kernel refused the bogus frame" grep -q "^WRITE-REJECTED" /tmp/dsd.log
kill -9 $STUCK 2>/dev/null
n=0
while kill -0 $STUCK 2>/dev/null; do
	n=$((n + 1)); [ $n -gt 100 ] && break; sleep 0.05
done
if kill -0 $STUCK 2>/dev/null; then
	fail "reader blocked by corrupt framing is still killable"
else
	ok "reader blocked by corrupt framing is still killable"
fi
wait $STUCK 2>/dev/null
stop

echo "  .. section 3"
# --- 3. abrupt death after successful use -----------------------------------
# Worth being precise about WHY the ordering matters: the connection dies when
# the LAST descriptor for it closes, not when the daemon process exits. The
# shell holds a duplicate (that is how the fd reaches mount), so both must go
# before the kernel sees a disconnect. Counting requests to time the death was
# too brittle — busybox `ls` stats every entry — so we kill outright.
start || { echo "  FAIL: mount"; exit 1; }
ls $MNT > /dev/null 2>&1
eq "served correctly before the death" "first file" "$(cat $MNT/one.txt)"
kill -9 $DSD_PID 2>/dev/null    # abrupt: no close(), no goodbye
exec 3>&-                       # ...and our duplicate goes too
sleep 0.3
check "operations fail after abrupt daemon death" \
	sh -c "! cat $MNT/one.txt 2>/dev/null"
check "unmount still works with a dead daemon" sh -c "umount $MNT"
ok "no wedge after abrupt death"

echo "  .. section 4"
# --- 4. concurrency: many writers, and notifications arriving underneath ----
start || { echo "  FAIL: mount"; exit 1; }
ls $MNT > /dev/null      # populate the dcache so notifications resolve

WPIDS=""
i=0
while [ $i -lt 8 ]; do
	( echo "writer $i payload" > $MNT/conc-$i.txt ) &
	WPIDS="$WPIDS $!"
	i=$((i + 1))
done
# ...while the daemon reports remote changes on the same directory.
j=0
while [ $j -lt 8 ]; do
	echo "create / remote-$j.txt x" > $CTL
	j=$((j + 1))
done
# Wait for the WRITERS specifically: a bare `wait` also waits on the daemon,
# which never exits.
for p in $WPIDS; do wait "$p" 2>/dev/null; done
sleep 0.5

made=$(ls $MNT | grep -c '^conc-')
eq "all concurrent writes landed" "8" "$made"
eq "a concurrent write kept its content" "writer 3 payload" "$(cat $MNT/conc-3.txt)"

echo "  .. section 5"
# --- 5. unmount while a request is in flight --------------------------------
stop
start --hang-on lookup || { echo "  FAIL: mount"; exit 1; }
cat $MNT/one.txt > /dev/null 2>&1 &
STUCK=$!
sleep 0.3
kill -9 $DSD_PID 2>/dev/null   # daemon dies with the request outstanding
exec 3>&-
sleep 0.4
kill -9 $STUCK 2>/dev/null
wait $STUCK 2>/dev/null
check "unmount after in-flight abort" sh -c "umount $MNT"
ok "in-flight request survived daemon death + unmount"

echo "  .. section 6"
# --- 6. module unload with everything gone ----------------------------------
rmmod ds_fs
check "rmmod clean" sh -c '! lsmod | grep -q ds_fs'

# --- 7. what the kernel's own checkers thought ------------------------------
# On the debug kernel this is where lockdep, DEBUG_OBJECTS and slab poisoning
# report. Anything here is a real defect, not a test artifact.
dmesg > /tmp/dmesg.txt
if grep -qE "BUG:|Oops|WARNING:|INFO: possible|held lock|Busy inodes|use-after-free|corrupt" /tmp/dmesg.txt; then
	echo "  --- kernel complaints ---"
	grep -nE "BUG:|Oops|WARNING:|INFO: possible|held lock|Busy inodes|use-after-free|corrupt" /tmp/dmesg.txt | head -20
	fail "kernel checkers reported nothing wrong"
else
	ok "kernel checkers reported nothing wrong"
fi

summary
