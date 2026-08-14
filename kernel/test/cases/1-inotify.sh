# Stage 1 GO/NO-GO: does an event this filesystem INJECTS (rather than one a
# syscall caused) reach a real inotify watcher, with the right mask, name and
# rename cookie? Everything else in the project is downstream of this answer.
. /tests/lib.sh

MNT=/mnt/dsfs
INJ=/proc/dsfs-inject

# inotify mask constants (uapi/linux/inotify.h)
M_MODIFY=00000002
M_ATTRIB=00000004
M_MOVED_FROM=00000040
M_MOVED_TO=00000080
M_CREATE=00000100
M_DELETE=00000200
M_DELETE_SELF=00000400
M_IGNORED=00008000
M_ISDIR=40000000

insmod /lib/modules/ds_fs.ko || { echo "  FAIL: insmod"; exit 1; }
mkdir -p $MNT
mount -t dsfs none $MNT || { echo "  FAIL: mount"; exit 1; }
check "module loaded"  grep -q dsfs /proc/filesystems
check "inject present" test -w $INJ

# The tree the module builds at mount time.
check "root lists a.txt" test -f $MNT/a.txt
check "root lists sub"   test -d $MNT/sub
eq    "a.txt contents" "hello world" "$(cat $MNT/a.txt)"
eq    "sub/b.txt contents" "bee" "$(cat $MNT/sub/b.txt)"

# Start a watcher on the mount root and on sub/, then inject. Wait for READY
# so the injection can't race the watch being installed.
watch_start() {  # watch_start <logfile> <paths...>
	log="$1"; shift
	ds-inotify -t 3 "$@" > "$log" 2>&1 &
	echo $! > /tmp/probe.pid
	n=0
	while ! grep -q '^READY' "$log" 2>/dev/null; do
		n=$((n + 1)); [ $n -gt 200 ] && break
		sleep 0.02
	done
}
watch_wait() { wait "$(cat /tmp/probe.pid)" 2>/dev/null; }

# --- 1/2: create + mkdir, watched on the parent directory -------------------
watch_start /tmp/ev1.log $MNT/sub
echo "create sub foo.txt" > $INJ
echo "mkdir sub newdir"   > $INJ
watch_wait
echo "  --- ev1 ---"; cat /tmp/ev1.log

check "CREATE reaches inotify"      grep -q "^EV 1 $M_CREATE 0 foo.txt$" /tmp/ev1.log
check "mkdir sets IN_ISDIR"         grep -q "^EV 1 40000100 0 newdir$" /tmp/ev1.log
check "injected name is visible"    test -f $MNT/sub/foo.txt
check "injected dir is visible"     test -d $MNT/sub/newdir

# --- 3: modify, watched on the DIRECTORY and on the FILE itself -------------
watch_start /tmp/ev2.log $MNT $MNT/a.txt
echo "modify / a.txt 4096" > $INJ
watch_wait
echo "  --- ev2 ---"; cat /tmp/ev2.log

check "MODIFY on dir watch (named)"  grep -q "^EV 1 $M_MODIFY 0 a.txt$" /tmp/ev2.log
check "MODIFY on file watch (self)"  grep -q "^EV 2 $M_MODIFY 0 -$" /tmp/ev2.log
# Caches must be honest before observers are woken.
eq "size visible at notify time" "4096" "$(stat -c %s $MNT/a.txt)"

# --- 4: attrib --------------------------------------------------------------
watch_start /tmp/ev3.log $MNT
echo "attrib / a.txt" > $INJ
watch_wait
check "ATTRIB reaches inotify" grep -q "^EV 1 $M_ATTRIB 0 a.txt$" /tmp/ev3.log

# --- 5: delete, watched on parent AND on the victim -------------------------
watch_start /tmp/ev4.log $MNT/sub $MNT/sub/foo.txt
echo "delete sub foo.txt" > $INJ
watch_wait
echo "  --- ev4 ---"; cat /tmp/ev4.log

check "DELETE on parent watch"  grep -q "^EV 1 $M_DELETE 0 foo.txt$" /tmp/ev4.log
check "DELETE_SELF on victim"   grep -q "^EV 2 $M_DELETE_SELF 0 -$" /tmp/ev4.log
check "IGNORED on victim"       grep -q "^EV 2 $M_IGNORED 0 -$" /tmp/ev4.log
check "deleted name is gone"    test ! -e $MNT/sub/foo.txt

# --- 6: THE ASSERTION THE PROJECT RESTS ON: rename cookie pairing -----------
watch_start /tmp/ev5.log $MNT/sub
echo "rename sub b.txt sub c.txt" > $INJ
watch_wait
echo "  --- ev5 ---"; cat /tmp/ev5.log

check "MOVED_FROM emitted" grep -q "^EV 1 $M_MOVED_FROM .* b.txt$" /tmp/ev5.log
check "MOVED_TO emitted"   grep -q "^EV 1 $M_MOVED_TO .* c.txt$" /tmp/ev5.log
cf=$(grep "^EV 1 $M_MOVED_FROM " /tmp/ev5.log | head -1 | cut -d' ' -f4)
ct=$(grep "^EV 1 $M_MOVED_TO " /tmp/ev5.log | head -1 | cut -d' ' -f4)
eq    "rename cookies match" "$cf" "$ct"
check "rename cookie nonzero" test "$cf" != "0"
check "new name resolves"     test -f $MNT/sub/c.txt
check "old name gone"         test ! -e $MNT/sub/b.txt

# --- 7: cross-directory rename lands on two different watches ---------------
watch_start /tmp/ev6.log $MNT $MNT/sub
echo "rename sub c.txt / moved.txt" > $INJ
watch_wait
echo "  --- ev6 ---"; cat /tmp/ev6.log

wd_root=$(grep "^WD .* $MNT$" /tmp/ev6.log | cut -d' ' -f2)
wd_sub=$(grep "^WD .* $MNT/sub$" /tmp/ev6.log | cut -d' ' -f2)
check "MOVED_FROM on source dir" grep -q "^EV $wd_sub $M_MOVED_FROM .* c.txt$" /tmp/ev6.log
check "MOVED_TO on target dir"   grep -q "^EV $wd_root $M_MOVED_TO .* moved.txt$" /tmp/ev6.log
cf=$(grep "^EV $wd_sub $M_MOVED_FROM " /tmp/ev6.log | head -1 | cut -d' ' -f4)
ct=$(grep "^EV $wd_root $M_MOVED_TO " /tmp/ev6.log | head -1 | cut -d' ' -f4)
eq "cross-dir cookies match" "$cf" "$ct"

# --- 8: cold cache — injecting on an unwatched, unwalked path must not crash
echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
echo "create / cold.txt" > $INJ
ok "cold-cache injection survived"

# --- 9: THE DIFFERENTIAL — the same operations on tmpfs, via real syscalls,
# must produce the same event transcript. This is the gold standard: it
# compares us against the kernel's own notion of what these events look like.
transcript() { cut -d' ' -f3,5 "$1" | grep -v '^$'; }

mkdir -p /tmp/ref
watch_start /tmp/ref.log /tmp/ref
touch /tmp/ref/x.txt
mv /tmp/ref/x.txt /tmp/ref/y.txt
rm /tmp/ref/y.txt
watch_wait

watch_start /tmp/ours.log $MNT/sub
echo "create sub x.txt" > $INJ
echo "rename sub x.txt sub y.txt" > $INJ
echo "delete sub y.txt" > $INJ
watch_wait

echo "  --- tmpfs (real syscalls) ---"; transcript /tmp/ref.log
echo "  --- dsfs (injected) ---";       transcript /tmp/ours.log
# tmpfs adds OPEN/CLOSE_WRITE from touch(1) actually opening the file; compare
# only the dirent events both sides should agree on.
ref=$(transcript /tmp/ref.log | grep -E "^($M_CREATE|$M_MOVED_FROM|$M_MOVED_TO|$M_DELETE) ")
ours=$(transcript /tmp/ours.log | grep -E "^($M_CREATE|$M_MOVED_FROM|$M_MOVED_TO|$M_DELETE) ")
eq "dirent transcript matches tmpfs" "$ref" "$ours"

# --- 10: clean teardown -----------------------------------------------------
check "no queue overflow" sh -c '! grep -q " 00004000 " /tmp/ev1.log /tmp/ev5.log'
umount $MNT
check "unmounted"  sh -c '! grep -q " dsfs " /proc/mounts'
rmmod ds_fs
check "rmmod clean" sh -c '! lsmod | grep -q ds_fs'

summary
