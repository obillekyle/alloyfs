# Stage 9: symlinks, hard links, and df.
#
# Symlinks were readable through a mount but not creatable, and the module had
# no ->get_link at all — an S_IFLNK inode would have been a mode the VFS could
# not resolve, so the daemon flattened links to regular files. This proves the
# whole path: symlink(2) reaches the agent, the target survives verbatim, the
# kernel resolves it on traversal, and a target that escapes the export is
# refused before anything is created.
. /tests/lib.sh

MNT=/mnt/alloyfs
EXPORT=/tmp/export
DATA=/tmp/alloyfs-data
ADDR=127.0.0.1:7440

export HOME=/tmp

insmod /lib/modules/alloyfs.ko || { echo "  FAIL: insmod"; exit 1; }
ip link set lo up 2>/dev/null || ifconfig lo up 2>/dev/null

mkdir -p $MNT $EXPORT/sub $DATA
echo "the real thing" > $EXPORT/real.txt
echo "nested" > $EXPORT/sub/inner.txt
# Something OUTSIDE the export, for the escape tests to aim at.
echo "not yours" > /tmp/outside.txt

alloyfs serve --tcp $ADDR --export test=$EXPORT > /tmp/agent.log 2>&1 &
AGENT_PID=$!
n=0
while ! grep -q 'listening' /tmp/agent.log 2>/dev/null; do
	n=$((n + 1)); [ $n -gt 400 ] && break
	sleep 0.05
done
check "agent listening" grep -q listening /tmp/agent.log

alloyfs mount tcp://$ADDR/test $MNT --backend kernel \
	--data-dir $DATA --auto-cache-max 0 > /tmp/mount.log 2>&1 &
MOUNT_PID=$!
n=0
while ! grep -q " alloyfs " /proc/mounts; do
	n=$((n + 1)); [ $n -gt 600 ] && break
	sleep 0.05
done
if ! grep -q " alloyfs " /proc/mounts; then
	echo "  FAIL: never mounted"; cat /tmp/mount.log; cat /tmp/agent.log; exit 1
fi
ok "mounted on the kernel backend"

# --- symlink: create, stat, resolve ----------------------------------------
ln -s real.txt $MNT/link.txt
check "symlink created"        test -L $MNT/link.txt
eq "target reads back"         "real.txt" "$(readlink $MNT/link.txt)"
eq "traversal resolves"        "the real thing" "$(cat $MNT/link.txt)"
check "it is a real symlink on the server" test -L $EXPORT/link.txt

# The kernel must see S_IFLNK, not a regular file — `test -L` above goes
# through the mount, this checks the mode the module actually reported.
eq "mode is a symlink"         "symbolic link" "$(stat -c %F $MNT/link.txt)"

# --- a relative target that walks up and back down --------------------------
ln -s ../real.txt $MNT/sub/up.txt
eq "relative target stored verbatim" "../real.txt" "$(readlink $MNT/sub/up.txt)"
eq "and resolves"                    "the real thing" "$(cat $MNT/sub/up.txt)"

# --- a dangling link is legal ----------------------------------------------
ln -s nowhere.txt $MNT/dangling.txt
check "dangling link exists"   test -L $MNT/dangling.txt
eq "dangling target readable"  "nowhere.txt" "$(readlink $MNT/dangling.txt)"
check "but does not resolve"   sh -c "! cat $MNT/dangling.txt 2>/dev/null"

# --- the export boundary ----------------------------------------------------
# Each of these must be refused, and must leave nothing behind.
for t in ".." "../../tmp/outside.txt" "/tmp/outside.txt"; do
	ln -s "$t" $MNT/escape.txt 2>/dev/null
	check "escaping target refused: $t" sh -c "! test -e $MNT/escape.txt -o -L $MNT/escape.txt"
	rm -f $MNT/escape.txt 2>/dev/null
done

# Landing ON the export root is inside it, so this one is allowed.
ln -s .. $MNT/sub/toroot
check "a link to the export root is allowed" test -L $MNT/sub/toroot

# --- hard links -------------------------------------------------------------
ln $MNT/real.txt $MNT/hard.txt
check "hard link created"   test -f $MNT/hard.txt
eq "same contents"          "the real thing" "$(cat $MNT/hard.txt)"

# One inode, two names — asserted ON THE SERVER, which is where the guarantee
# actually lives.
echo "changed via the hard link" > $MNT/hard.txt
eq "server-side inode is shared" "changed via the hard link" "$(cat $EXPORT/real.txt)"
eq "and the link count is 2"     "2" "$(stat -c %h $EXPORT/real.txt)"

# NOT asserted: that reading the OTHER name through the mount immediately
# shows the new bytes. It does not, and this is a real limitation rather than
# an oversight. The client keys its caches by path, so it cannot know two
# names share an inode; the daemon holds a readahead window per nodeid, and a
# write through one name leaves the other's window untouched. Our own writes
# are stripped from the event stream (self-origin), so nothing arrives to
# invalidate it either. Fixing it properly needs a stable inode identity on
# the wire, which the protocol does not carry. NFS has the same hole.
# Documented in the README under hard links.

# --- statfs -----------------------------------------------------------------
# simple_statfs reported zeroes, which makes df show a 0-byte volume and makes
# anything checking free space refuse to write.
BLOCKS=$(stat -f -c %b $MNT)
check "statfs reports a nonzero size" sh -c "[ \"$BLOCKS\" -gt 0 ]"
FREE=$(stat -f -c %a $MNT)
check "and nonzero free space"        sh -c "[ \"$FREE\" -gt 0 ]"

# --- teardown ---------------------------------------------------------------
kill -INT $MOUNT_PID 2>/dev/null
n=0
while grep -q ' alloyfs ' /proc/mounts; do
	n=$((n + 1)); [ $n -gt 200 ] && break
	sleep 0.05
done
check "unmounted" sh -c '! grep -q " alloyfs " /proc/mounts'
kill -9 $AGENT_PID 2>/dev/null

# The client unmounts lazily (MNT_DETACH), so an empty /proc/mounts proves
# nothing about what still pins the module: the superblock is freed
# asynchronously, and the client holds /dev/alloyfs open until its serving
# thread has wound down. A fixed sleep turns that into a coin toss decided by
# how fast the machine is — retry instead, as stage 6 already does.
n=0
while lsmod | grep -q alloyfs; do
	rmmod alloyfs 2>/dev/null && break
	n=$((n + 1)); [ $n -gt 50 ] && break
	sleep 0.1
done
check "rmmod clean" sh -c '! lsmod | grep -q alloyfs'

summary
