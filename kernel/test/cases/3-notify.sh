# Stage 3: the payoff. A DAEMON reports a remote change over /dev/alloyfs and
# real inotify watchers on the mount see it — no syscall touched those files.
# This is the thing FUSE structurally cannot do.
#
# Everything else in the project is downstream of the answer this stage gives,
# which is why the whole set lives here: the mask, the name, the rename cookie
# pairing, the events a watch on the FILE itself must get, what happens when
# the kernel has never heard of the path, and a transcript compared against
# tmpfs doing the same work through real syscalls.
. /tests/lib.sh

MNT=/mnt/alloyfs
CTL=/tmp/alloyd.ctl

# inotify mask constants (uapi/linux/inotify.h)
M_MODIFY=00000002
M_ATTRIB=00000004
M_MOVED_FROM=00000040
M_MOVED_TO=00000080
M_CREATE=00000100
M_DELETE=00000200
M_DELETE_SELF=00000400
M_Q_OVERFLOW=00004000
M_IGNORED=00008000

insmod /lib/modules/alloyfs.ko || { echo "  FAIL: insmod"; exit 1; }
mkdir -p $MNT
mkfifo $CTL 2>/dev/null

exec 3<> /dev/alloyfs || { echo "  FAIL: cannot open /dev/alloyfs"; exit 1; }
alloyd --fd 3 --ctl $CTL > /tmp/alloyd.log 2>&1 &
ALLOYD_PID=$!
sleep 0.3
mount -t alloyfs -o fd=3 none $MNT || { echo "  FAIL: mount"; cat /tmp/alloyd.log; exit 1; }
ok "daemon-backed mount up"

# The kernel only notifies on paths it has cached — an unwalked subtree
# cannot be under a watch. Walk the tree first, as any real user would.
ls $MNT > /dev/null
ls $MNT/dir > /dev/null

watch_start() {  # watch_start <logfile> <paths...>
	log="$1"; shift
	alloyfs-inotify -t 3 "$@" > "$log" 2>&1 &
	echo $! > /tmp/probe.pid
	n=0
	while ! grep -q '^READY' "$log" 2>/dev/null; do
		n=$((n + 1)); [ $n -gt 200 ] && break
		sleep 0.02
	done
}
watch_wait() { wait "$(cat /tmp/probe.pid)" 2>/dev/null; }

# --- create / mkdir ---------------------------------------------------------
# Look both names up BEFORE they exist. Each miss is cached as a negative
# dentry and nothing revalidates one, so a create that fails to invalidate it
# leaves a file that exists on the server permanently invisible here — a
# silent wrong answer, not an error.
check "names absent before the events" \
	sh -c "! test -e $MNT/fresh.txt && ! test -e $MNT/freshdir"

watch_start /tmp/n1.log $MNT
echo "create / fresh.txt hello" > $CTL
echo "mkdir / freshdir" > $CTL
watch_wait
echo "  --- n1 ---"; cat /tmp/n1.log

check "daemon CREATE reaches inotify" grep -q "^EV 1 $M_CREATE 0 fresh.txt$" /tmp/n1.log
check "daemon mkdir sets ISDIR"       grep -q "^EV 1 40000100 0 freshdir$" /tmp/n1.log
# ...and the names are really there, served by the daemon on the next lookup.
check "created file is visible" test -f $MNT/fresh.txt
check "created dir is visible"  test -d $MNT/freshdir
eq "created file readable" "hello" "$(cat $MNT/fresh.txt)"

# --- modify: caches honest before observers are woken -----------------------
# The self-event is the load-bearing one: it can only come from the branch
# that found the cached inode, which is the same branch that writes the new
# size and drops the stale pages. A watcher that reacts by reading gets the
# new contents, not the ones the event was announcing the end of.
watch_start /tmp/n2.log $MNT $MNT/one.txt
echo "modify / one.txt a-much-longer-body" > $CTL
watch_wait
echo "  --- n2 ---"; cat /tmp/n2.log

check "MODIFY on dir watch"  grep -q "^EV 1 $M_MODIFY 0 one.txt$" /tmp/n2.log
check "MODIFY on file watch" grep -q "^EV 2 $M_MODIFY 0 -$" /tmp/n2.log
eq "new size visible"     "18" "$(stat -c %s $MNT/one.txt)"
eq "new contents visible" "a-much-longer-body" "$(cat $MNT/one.txt)"

# --- attrib -----------------------------------------------------------------
watch_start /tmp/n3.log $MNT
echo "attrib / one.txt" > $CTL
watch_wait
check "ATTRIB reaches inotify" grep -q "^EV 1 $M_ATTRIB 0 one.txt$" /tmp/n3.log

# --- delete, watched on the parent AND on the victim ------------------------
# A watch on the file itself is the case that catches events being routed to
# the wrong inode: the parent's DELETE can look perfect while the file's own
# watch never learns it is gone, and a watcher waiting for DELETE_SELF then
# waits forever. IGNORED must follow, because the mark itself is finished.
watch_start /tmp/n4.log $MNT $MNT/fresh.txt
echo "delete / fresh.txt" > $CTL
watch_wait
echo "  --- n4 ---"; cat /tmp/n4.log

check "DELETE reaches inotify" grep -q "^EV 1 $M_DELETE 0 fresh.txt$" /tmp/n4.log
check "DELETE_SELF on victim"  grep -q "^EV 2 $M_DELETE_SELF 0 -$" /tmp/n4.log
check "IGNORED on victim"      grep -q "^EV 2 $M_IGNORED 0 -$" /tmp/n4.log
check "deleted name is gone"   sh -c "! cat $MNT/fresh.txt 2>/dev/null"

# --- rename: the cookie pairing, over the real transport --------------------
watch_start /tmp/n5.log $MNT
echo "rename / one.txt / renamed.txt" > $CTL
watch_wait
echo "  --- n5 ---"; cat /tmp/n5.log

check "MOVED_FROM emitted" grep -q "^EV 1 $M_MOVED_FROM .* one.txt$" /tmp/n5.log
check "MOVED_TO emitted"   grep -q "^EV 1 $M_MOVED_TO .* renamed.txt$" /tmp/n5.log
cf=$(grep "^EV 1 $M_MOVED_FROM " /tmp/n5.log | head -1 | cut -d' ' -f4)
ct=$(grep "^EV 1 $M_MOVED_TO " /tmp/n5.log | head -1 | cut -d' ' -f4)
eq    "rename cookies match"  "$cf" "$ct"
check "rename cookie nonzero" test "$cf" != "0"
eq    "new name serves content" "a-much-longer-body" "$(cat $MNT/renamed.txt)"
# The old dentry was positive and its inode still exists on the server under
# the new name, so a stale one would keep answering with the file's contents.
check "old name gone" sh -c "! cat $MNT/one.txt 2>/dev/null"

# --- cross-directory rename -------------------------------------------------
watch_start /tmp/n6.log $MNT $MNT/dir
echo "rename / renamed.txt dir moved.txt" > $CTL
watch_wait
echo "  --- n6 ---"; cat /tmp/n6.log
wd_root=$(grep "^WD .* $MNT$" /tmp/n6.log | cut -d' ' -f2)
wd_sub=$(grep "^WD .* $MNT/dir$" /tmp/n6.log | cut -d' ' -f2)
check "MOVED_FROM on source dir" grep -q "^EV $wd_root $M_MOVED_FROM .* renamed.txt$" /tmp/n6.log
check "MOVED_TO on target dir"   grep -q "^EV $wd_sub $M_MOVED_TO .* moved.txt$" /tmp/n6.log
cf=$(grep "^EV $wd_root $M_MOVED_FROM " /tmp/n6.log | head -1 | cut -d' ' -f4)
ct=$(grep "^EV $wd_sub $M_MOVED_TO " /tmp/n6.log | head -1 | cut -d' ' -f4)
eq "cross-dir cookies match" "$cf" "$ct"

# --- THE DIFFERENTIAL — the same three operations on tmpfs, through real
# syscalls, must produce the same event transcript. This is the gold standard:
# it compares us against the kernel's own notion of what these events look
# like, rather than against what this project expected them to look like.
transcript() { cut -d' ' -f3,5 "$1" | grep -v '^$'; }

mkdir -p /tmp/ref
watch_start /tmp/ref.log /tmp/ref
touch /tmp/ref/x.txt
mv /tmp/ref/x.txt /tmp/ref/y.txt
rm /tmp/ref/y.txt
watch_wait

watch_start /tmp/ours.log $MNT/dir
echo "create dir x.txt" > $CTL
echo "rename dir x.txt dir y.txt" > $CTL
echo "delete dir y.txt" > $CTL
watch_wait

echo "  --- tmpfs (real syscalls) ---"; transcript /tmp/ref.log
echo "  --- alloyfs (from the daemon) ---"; transcript /tmp/ours.log
# tmpfs adds OPEN/CLOSE_WRITE from touch(1) actually opening the file; compare
# only the dirent events both sides should agree on.
ref=$(transcript /tmp/ref.log | grep -E "^($M_CREATE|$M_MOVED_FROM|$M_MOVED_TO|$M_DELETE) ")
ours=$(transcript /tmp/ours.log | grep -E "^($M_CREATE|$M_MOVED_FROM|$M_MOVED_TO|$M_DELETE) ")
eq "dirent transcript matches tmpfs" "$ref" "$ours"

# --- notifications the kernel cannot place ----------------------------------
# Nothing has walked this directory, so ilookup() finds no inode for it. The
# notification must be dropped without a word, not treated as an error and
# certainly not resolved against a NULL dentry.
echo "mkdir / unseen" > $CTL
echo "create unseen deep.txt" > $CTL
sleep 0.2
ok "notification for an unwalked parent did not crash"

# Same path, from the other direction: everything evictable is thrown away
# first, so this is a parent that WAS cached and no longer is.
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
echo "create / cold.txt" > $CTL
sleep 0.2
ok "cold-cache notification survived"

# Nothing above may have overflowed a watcher's queue: an overflow would mean
# the events checked for were merely the ones that happened to survive.
check "no queue overflow" \
	sh -c "! grep -q ' $M_Q_OVERFLOW ' /tmp/n1.log /tmp/n4.log /tmp/n5.log /tmp/ours.log"

# --- teardown ---------------------------------------------------------------
kill -9 $ALLOYD_PID 2>/dev/null
exec 3>&-
sleep 0.2
umount $MNT
check "unmounted" sh -c '! grep -q " alloyfs " /proc/mounts'
rmmod alloyfs
check "rmmod clean" sh -c '! lsmod | grep -q alloyfs'

summary
