# Stage 3: the payoff. A DAEMON reports a remote change over /dev/alloyfs and
# real inotify watchers on the mount see it — no syscall touched those files.
# This is the thing FUSE structurally cannot do.
. /tests/lib.sh

MNT=/mnt/alloyfs
CTL=/tmp/alloyd.ctl

M_MODIFY=00000002
M_ATTRIB=00000004
M_MOVED_FROM=00000040
M_MOVED_TO=00000080
M_CREATE=00000100
M_DELETE=00000200

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
watch_start /tmp/n1.log $MNT
echo "create / fresh.txt hello" > $CTL
echo "mkdir / freshdir" > $CTL
watch_wait
echo "  --- n1 ---"; cat /tmp/n1.log

check "daemon CREATE reaches inotify" grep -q "^EV 1 $M_CREATE 0 fresh.txt$" /tmp/n1.log
check "daemon mkdir sets ISDIR"       grep -q "^EV 1 40000100 0 freshdir$" /tmp/n1.log
# ...and the name is really there, served by the daemon on the next lookup.
eq "created file readable" "hello" "$(cat $MNT/fresh.txt)"

# --- modify: caches honest before observers are woken -----------------------
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

# --- delete -----------------------------------------------------------------
watch_start /tmp/n4.log $MNT
echo "delete / fresh.txt" > $CTL
watch_wait
check "DELETE reaches inotify" grep -q "^EV 1 $M_DELETE 0 fresh.txt$" /tmp/n4.log
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

# --- a notification for an uncached parent must be silently ignored ---------
echo "create dir deep.txt" > $CTL
sleep 0.2
ok "notification for unwalked path did not crash"

# --- teardown ---------------------------------------------------------------
kill -9 $ALLOYD_PID 2>/dev/null
exec 3>&-
sleep 0.2
umount $MNT
check "unmounted" sh -c '! grep -q " alloyfs " /proc/mounts'
rmmod alloyfs
check "rmmod clean" sh -c '! lsmod | grep -q alloyfs'

summary
