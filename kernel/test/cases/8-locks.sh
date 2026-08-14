# Stage 8: advisory locks that mean something between machines.
#
# Before this the module implemented neither ->lock nor ->flock, so the VFS
# granted every request from its own list and never told the agent. Two mounts
# of the same export each held "the" exclusive lock and both were told yes.
# That is the failure this stage exists to make impossible: it mounts the SAME
# export twice, as two independent clients with their own data dirs, and
# asserts they exclude each other. A single mount could not distinguish a
# forwarded lock from the local-only bug.
. /tests/lib.sh

MNT=/mnt/alloyfs
MNT2=/mnt/alloyfs2
EXPORT=/tmp/export
DATA=/tmp/alloyfs-data
DATA2=/tmp/alloyfs-data2
ADDR=127.0.0.1:7440

export HOME=/tmp

M_MODIFY=00000002

check "lock tool in the image" test -x /bin/alloyfs-lock

insmod /lib/modules/alloyfs.ko || { echo "  FAIL: insmod"; exit 1; }
ip link set lo up 2>/dev/null || ifconfig lo up 2>/dev/null

mkdir -p $MNT $MNT2 $EXPORT $DATA $DATA2
echo "contended" > $EXPORT/lockme.txt
echo "other" > $EXPORT/other.txt

alloyfs serve --tcp $ADDR --export test=$EXPORT > /tmp/agent.log 2>&1 &
AGENT_PID=$!
n=0
while ! grep -q 'listening' /tmp/agent.log 2>/dev/null; do
	n=$((n + 1)); [ $n -gt 400 ] && break
	sleep 0.05
done
check "agent listening" grep -q listening /tmp/agent.log

# Two clients, two data dirs. Sharing a data dir would make them one client
# wearing two hats and the contention below would prove nothing.
alloyfs mount tcp://$ADDR/test $MNT --backend kernel \
	--data-dir $DATA --auto-cache-max 0 > /tmp/mount1.log 2>&1 &
MOUNT1=$!
alloyfs mount tcp://$ADDR/test $MNT2 --backend kernel \
	--data-dir $DATA2 --auto-cache-max 0 > /tmp/mount2.log 2>&1 &
MOUNT2=$!
n=0
while [ "$(grep -c ' alloyfs ' /proc/mounts)" -lt 2 ]; do
	n=$((n + 1)); [ $n -gt 800 ] && break
	sleep 0.05
done
if [ "$(grep -c ' alloyfs ' /proc/mounts)" -lt 2 ]; then
	echo "  FAIL: both mounts never came up"
	cat /tmp/mount1.log /tmp/mount2.log /tmp/agent.log
	exit 1
fi
ok "two independent mounts of one export"
eq "both see the file" "contended" "$(cat $MNT2/lockme.txt)"

# --- the headline: an exclusive lock crosses the mount boundary -------------
alloyfs-lock -x -w 4000 $MNT/lockme.txt > /tmp/h1.log 2>&1 &
H1=$!
n=0; while ! grep -q . /tmp/h1.log 2>/dev/null; do
	n=$((n + 1)); [ $n -gt 400 ] && break; sleep 0.02
done
eq "mount A takes the lock" "LOCKED" "$(cat /tmp/h1.log)"
eq "mount B is refused"     "BUSY"   "$(alloyfs-lock -x -n $MNT2/lockme.txt 2>&1)"

# An unrelated file must stay free — a lock that excludes everything would
# also pass the test above.
eq "a different file is unaffected" "LOCKED" "$(alloyfs-lock -x -n $MNT2/other.txt 2>&1)"

# --- a held lock survives a remote change ----------------------------------
# The daemon refreshes a handle's cached bytes when the file changes under it.
# That refresh used to release the handle, which would have quietly dropped
# this lock.
echo "changed behind both mounts" > $EXPORT/lockme.txt
sleep 0.6
eq "lock survives a remote modify" "BUSY" "$(alloyfs-lock -x -n $MNT2/lockme.txt 2>&1)"
eq "and the new bytes are visible" "changed behind both mounts" "$(cat $MNT2/lockme.txt)"

# --- same-machine exclusion still works ------------------------------------
# Both local processes share ONE daemon handle, so the server cannot tell them
# apart; this passes only because the module takes the VFS lock first.
eq "same mount, second process refused" "BUSY" "$(alloyfs-lock -x -n $MNT/lockme.txt 2>&1)"

# --- releasing on close hands it over --------------------------------------
wait $H1 2>/dev/null
eq "free once the holder exits" "LOCKED" "$(alloyfs-lock -x -n $MNT2/lockme.txt 2>&1)"

# --- shared locks coexist across mounts ------------------------------------
alloyfs-lock -s -w 2000 $MNT/lockme.txt > /tmp/h2.log 2>&1 &
H2=$!
n=0; while ! grep -q . /tmp/h2.log 2>/dev/null; do
	n=$((n + 1)); [ $n -gt 400 ] && break; sleep 0.02
done
eq "reader on A"                "LOCKED" "$(cat /tmp/h2.log)"
eq "reader on B joins"          "LOCKED" "$(alloyfs-lock -s -n $MNT2/lockme.txt 2>&1)"
eq "writer on B still refused"  "BUSY"   "$(alloyfs-lock -x -n $MNT2/lockme.txt 2>&1)"
wait $H2 2>/dev/null

# --- flock(2) goes through ->flock, a different hook ------------------------
alloyfs-lock -f -x -w 2500 $MNT/lockme.txt > /tmp/h3.log 2>&1 &
H3=$!
n=0; while ! grep -q . /tmp/h3.log 2>/dev/null; do
	n=$((n + 1)); [ $n -gt 400 ] && break; sleep 0.02
done
eq "flock on A"          "LOCKED" "$(cat /tmp/h3.log)"
eq "flock on B refused"  "BUSY"   "$(alloyfs-lock -f -x -n $MNT2/lockme.txt 2>&1)"

# --- F_SETLKW waits rather than failing ------------------------------------
# The daemon dispatches each request on its own blocking task, so a sleeper
# must not wedge the mount: the read below has to answer while B is waiting.
alloyfs-lock -f -x $MNT2/lockme.txt > /tmp/h4.log 2>&1 &
H4=$!
sleep 0.3
eq "mount stays responsive under a waiter" "other" "$(cat $MNT/other.txt)"
check "waiter has not been granted yet" sh -c "! grep -q LOCKED /tmp/h4.log"
wait $H3 2>/dev/null
wait $H4 2>/dev/null
eq "waiter is granted once the holder leaves" "LOCKED" "$(cat /tmp/h4.log)"

# --- teardown ---------------------------------------------------------------
kill -INT $MOUNT1 $MOUNT2 2>/dev/null
n=0
while grep -q ' alloyfs ' /proc/mounts; do
	n=$((n + 1)); [ $n -gt 200 ] && break
	sleep 0.05
done
check "both unmounted" sh -c '! grep -q " alloyfs " /proc/mounts'
kill -9 $AGENT_PID 2>/dev/null
sleep 0.3
rmmod alloyfs
check "rmmod clean" sh -c '! lsmod | grep -q alloyfs'

summary
