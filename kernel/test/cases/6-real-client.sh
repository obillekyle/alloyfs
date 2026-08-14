# Stage 6: the C test daemon is gone. A real `alloyfs serve` agent exports a
# real directory, a real `alloyfs mount --backend kernel` serves it through the
# module, and everything stages 2-4 proved with alloyd is proved again with the
# actual product on both ends.
#
# The payoff assertion is the same one, and it is the one no FUSE mount can
# make: something changes the exported directory BEHIND the mount's back, and a
# plain inotify watcher on the mount sees a genuine kernel event for it. The
# whole chain is real here — inotify on the server, the event stream, the
# client, the notification ABI, fsnotify in the module.
. /tests/lib.sh

MNT=/mnt/alloyfs
EXPORT=/tmp/export
DATA=/tmp/alloyfs-data
ADDR=127.0.0.1:7440

# The client keeps per-user state under $HOME/.alloyfs; the guest has no
# passwd file, so say where that goes rather than leaving it to a fallback.
export HOME=/tmp

M_MODIFY=00000002
M_CREATE=00000100
M_DELETE=00000200
M_MOVED_FROM=00000040
M_MOVED_TO=00000080

check "client binary in the image" test -x /bin/alloyfs

insmod /lib/modules/alloyfs.ko || { echo "  FAIL: insmod"; exit 1; }
check "device created" test -c /dev/alloyfs

# The client reaches the agent over TCP, both inside this guest. Bringing
# loopback up is enough: the kernel gives lo 127.0.0.1 itself.
ip link set lo up 2>/dev/null || ifconfig lo up 2>/dev/null

mkdir -p $MNT $EXPORT/dir $DATA
echo "first file" > $EXPORT/one.txt
echo "second file, in a subdirectory" > $EXPORT/dir/two.txt

# --- a real agent ------------------------------------------------------------
alloyfs serve --tcp $ADDR --export test=$EXPORT > /tmp/agent.log 2>&1 &
AGENT_PID=$!
n=0
while ! grep -q 'listening' /tmp/agent.log 2>/dev/null; do
	n=$((n + 1)); [ $n -gt 400 ] && break
	sleep 0.05
done
if ! grep -q 'listening' /tmp/agent.log 2>/dev/null; then
	echo "  FAIL: agent never listened"; cat /tmp/agent.log; exit 1
fi
ok "agent listening on $ADDR"

# --- and a real client, on the kernel backend --------------------------------
# --auto-cache-max 0 keeps the local blob cache (and its walker) out of the
# picture: this stage is about the kernel path, not about caching.
alloyfs mount tcp://$ADDR/test $MNT --backend kernel \
	--data-dir $DATA --auto-cache-max 0 > /tmp/mount.log 2>&1 &
MOUNT_PID=$!
n=0
while ! grep -q " alloyfs " /proc/mounts; do
	n=$((n + 1)); [ $n -gt 600 ] && break
	sleep 0.05
done
if ! grep -q " alloyfs " /proc/mounts; then
	echo "  FAIL: the client never mounted"; cat /tmp/mount.log; cat /tmp/agent.log; exit 1
fi
ok "mounted by the Rust client via the kernel module"

# --- the tree comes from the agent, over the module (stage 2, for real) ------
eq "root listing" "dir
one.txt" "$(ls $MNT | sort)"
check "subdir listed"   test -d $MNT/dir
eq "file contents"      "first file" "$(cat $MNT/one.txt)"
eq "nested contents"    "second file, in a subdirectory" "$(cat $MNT/dir/two.txt)"
eq "size from getattr"  "11" "$(stat -c %s $MNT/one.txt)"
check "missing name is ENOENT" sh -c "! cat $MNT/nope.txt 2>/dev/null"

# --- local writes travel to the server (stage 4, for real) -------------------
echo "written locally" > $MNT/new.txt
check "create reached the export"  test -f $EXPORT/new.txt
eq "content round-trips"           "written locally" "$(cat $EXPORT/new.txt)"
eq "and reads back through the mount" "written locally" "$(cat $MNT/new.txt)"

echo "second line" >> $MNT/new.txt
eq "append works" "written locally
second line" "$(cat $EXPORT/new.txt)"

: > $MNT/new.txt
eq "truncate to zero" "0" "$(stat -c %s $EXPORT/new.txt)"

mkdir $MNT/newdir
check "mkdir reached the export" test -d $EXPORT/newdir
mv $MNT/new.txt $MNT/newdir/moved.txt
check "rename: old name gone"  sh -c "! test -f $EXPORT/new.txt"
check "rename: new name there" test -f $EXPORT/newdir/moved.txt
rm $MNT/newdir/moved.txt
check "unlink reached the export" sh -c "! test -f $EXPORT/newdir/moved.txt"
rmdir $MNT/newdir
check "rmdir reached the export"  sh -c "! test -d $EXPORT/newdir"

# --- a larger-than-one-request write ----------------------------------------
# One 256 KiB write() is two full 128 KiB payloads, so the chunking on both
# sides of the ABI runs for real instead of one tidy request.
dd if=/dev/zero of=$MNT/big.bin bs=256k count=1 2>/dev/null
eq "256 KiB write is whole on the server" "262144" "$(stat -c %s $EXPORT/big.bin)"
eq "and reads back at full length"        "262144" "$(wc -c < $MNT/big.bin)"
rm $MNT/big.bin

# --- THE POINT: a change behind the mount's back becomes a real inotify event
# The kernel only notifies on paths it has cached, so walk the tree first —
# which is also what any real watcher would have done.
ls $MNT > /dev/null
ls $MNT/dir > /dev/null

watch_start() {  # watch_start <logfile> <paths...>
	log="$1"; shift
	alloyfs-inotify -t 8 "$@" > "$log" 2>&1 &
	echo $! > /tmp/probe.pid
	n=0
	while ! grep -q '^READY' "$log" 2>/dev/null; do
		n=$((n + 1)); [ $n -gt 400 ] && break
		sleep 0.02
	done
}
watch_wait() { wait "$(cat /tmp/probe.pid)" 2>/dev/null; }

watch_start /tmp/n1.log $MNT
echo hello > $EXPORT/fresh.txt        # nothing touched the mount
mkdir $EXPORT/freshdir
watch_wait
echo "  --- n1 ---"; cat /tmp/n1.log
check "server-side create reaches inotify" grep -q "^EV 1 $M_CREATE 0 fresh.txt$" /tmp/n1.log
check "server-side mkdir sets ISDIR"       grep -q "^EV 1 40000100 0 freshdir$" /tmp/n1.log
eq "and the new file is readable"          "hello" "$(cat $MNT/fresh.txt)"

watch_start /tmp/n2.log $MNT $MNT/one.txt
echo "a-much-longer-body" > $EXPORT/one.txt
watch_wait
echo "  --- n2 ---"; cat /tmp/n2.log
check "MODIFY on the directory watch" grep -q "^EV 1 $M_MODIFY 0 one.txt$" /tmp/n2.log
check "MODIFY on the file watch"      grep -q "^EV 2 $M_MODIFY 0 -$" /tmp/n2.log
eq "new size visible"     "19" "$(stat -c %s $MNT/one.txt)"
eq "new contents visible" "a-much-longer-body" "$(cat $MNT/one.txt)"

watch_start /tmp/n3.log $MNT
rm $EXPORT/fresh.txt
watch_wait
echo "  --- n3 ---"; cat /tmp/n3.log
check "server-side delete reaches inotify" grep -q "^EV 1 $M_DELETE 0 fresh.txt$" /tmp/n3.log
check "and the name is gone"               sh -c "! cat $MNT/fresh.txt 2>/dev/null"

# A rename must arrive as ONE move (paired cookie), not as a delete + create.
watch_start /tmp/n4.log $MNT
mv $EXPORT/one.txt $EXPORT/renamed.txt
watch_wait
echo "  --- n4 ---"; cat /tmp/n4.log
check "MOVED_FROM emitted" grep -q "^EV 1 $M_MOVED_FROM .* one.txt$" /tmp/n4.log
check "MOVED_TO emitted"   grep -q "^EV 1 $M_MOVED_TO .* renamed.txt$" /tmp/n4.log
cf=$(grep "^EV 1 $M_MOVED_FROM " /tmp/n4.log | head -1 | cut -d' ' -f4)
ct=$(grep "^EV 1 $M_MOVED_TO " /tmp/n4.log | head -1 | cut -d' ' -f4)
eq    "rename cookies match"  "$cf" "$ct"
check "rename cookie nonzero" test "$cf" != "0"
eq    "new name serves content" "a-much-longer-body" "$(cat $MNT/renamed.txt)"

# --- and a LOCAL change still produces the VFS's own events ------------------
watch_start /tmp/n5.log $MNT
echo local > $MNT/watched.txt
rm $MNT/watched.txt
watch_wait
echo "  --- n5 ---"; cat /tmp/n5.log
check "local create notified" grep -q "^EV 1 $M_CREATE 0 watched.txt$" /tmp/n5.log
check "local delete notified" grep -q "^EV 1 $M_DELETE 0 watched.txt$" /tmp/n5.log

# --- teardown: SIGINT must unmount cleanly -----------------------------------
kill -INT $MOUNT_PID 2>/dev/null
n=0
while grep -q " alloyfs " /proc/mounts; do
	n=$((n + 1)); [ $n -gt 200 ] && break
	sleep 0.05
done
check "SIGINT unmounted the filesystem" sh -c '! grep -q " alloyfs " /proc/mounts'
if grep -q " alloyfs " /proc/mounts; then
	umount $MNT 2>/dev/null       # do not leave the module pinned
fi
kill -9 $MOUNT_PID 2>/dev/null
kill -9 $AGENT_PID 2>/dev/null

# The superblock is freed asynchronously after a lazy umount, and the module
# stays pinned until it is — so retry rather than racing it.
n=0
while lsmod | grep -q alloyfs; do
	rmmod alloyfs 2>/dev/null && break
	n=$((n + 1)); [ $n -gt 50 ] && break
	sleep 0.1
done
check "rmmod clean" sh -c '! lsmod | grep -q alloyfs'

summary
