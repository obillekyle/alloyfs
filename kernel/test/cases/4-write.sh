# Stage 4: the write path. Local mutations travel to the daemon and come
# back true — and the VFS emits its own fsnotify for them, so a local create
# and a remote one are indistinguishable to a watcher. That symmetry is the
# point: applications should not be able to tell which side made a change.
. /tests/lib.sh

MNT=/mnt/dsfs
CTL=/tmp/dsd.ctl

M_MODIFY=00000002
M_CREATE=00000100
M_DELETE=00000200

insmod /lib/modules/ds_fs.ko || { echo "  FAIL: insmod"; exit 1; }
mkdir -p $MNT
mkfifo $CTL 2>/dev/null

exec 3<> /dev/ds-fs || { echo "  FAIL: cannot open /dev/ds-fs"; exit 1; }
dsd --fd 3 --ctl $CTL > /tmp/dsd.log 2>&1 &
DSD_PID=$!
sleep 0.3
mount -t dsfs -o fd=3 none $MNT || { echo "  FAIL: mount"; cat /tmp/dsd.log; exit 1; }
ok "daemon-backed mount up"

# --- create + write + read back ---------------------------------------------
echo "written locally" > $MNT/new.txt
check "create succeeded"  test -f $MNT/new.txt
eq "content round-trips"  "written locally" "$(cat $MNT/new.txt)"
eq "size is right"        "16" "$(stat -c %s $MNT/new.txt)"

# The daemon really has it — prove the write left the kernel.
eq "daemon serves it after remount-free re-read" "written locally" "$(cat $MNT/new.txt)"

# --- append -----------------------------------------------------------------
echo "second line" >> $MNT/new.txt
eq "append works" "written locally
second line" "$(cat $MNT/new.txt)"

# --- truncate ---------------------------------------------------------------
: > $MNT/new.txt
eq "truncate to zero" "0" "$(stat -c %s $MNT/new.txt)"

# --- mkdir / rmdir ----------------------------------------------------------
mkdir $MNT/newdir
check "mkdir"        test -d $MNT/newdir
echo hi > $MNT/newdir/inner.txt
check "file in new dir" test -f $MNT/newdir/inner.txt
check "rmdir refuses non-empty" sh -c "! rmdir $MNT/newdir 2>/dev/null"
rm $MNT/newdir/inner.txt
check "unlink"       sh -c "! test -f $MNT/newdir/inner.txt"
rmdir $MNT/newdir
check "rmdir"        sh -c "! test -d $MNT/newdir"

# --- rename -----------------------------------------------------------------
echo movable > $MNT/from.txt
mv $MNT/from.txt $MNT/to.txt
check "old name gone"  sh -c "! test -f $MNT/from.txt"
eq "new name has content" "movable" "$(cat $MNT/to.txt)"

mkdir $MNT/dest
mv $MNT/to.txt $MNT/dest/moved.txt
eq "cross-dir rename" "movable" "$(cat $MNT/dest/moved.txt)"

# --- a LOCAL write produces fsnotify from the VFS, unaided ------------------
ds-inotify -t 3 $MNT > /tmp/w1.log 2>&1 &
P=$!
n=0; while ! grep -q '^READY' /tmp/w1.log 2>/dev/null; do n=$((n+1)); [ $n -gt 200 ] && break; sleep 0.02; done
echo local > $MNT/watched.txt
rm $MNT/watched.txt
wait $P 2>/dev/null
echo "  --- w1 ---"; cat /tmp/w1.log
check "local create notified"  grep -q "^EV 1 $M_CREATE 0 watched.txt$" /tmp/w1.log
check "local modify notified"  grep -q "^EV 1 $M_MODIFY 0 watched.txt$" /tmp/w1.log
check "local delete notified"  grep -q "^EV 1 $M_DELETE 0 watched.txt$" /tmp/w1.log

# --- and a REMOTE change is indistinguishable to the same watcher -----------
ds-inotify -t 3 $MNT > /tmp/w2.log 2>&1 &
P=$!
n=0; while ! grep -q '^READY' /tmp/w2.log 2>/dev/null; do n=$((n+1)); [ $n -gt 200 ] && break; sleep 0.02; done
echo "create / from-server.txt hello" > $CTL
wait $P 2>/dev/null
check "remote create notified identically" grep -q "^EV 1 $M_CREATE 0 from-server.txt$" /tmp/w2.log

# --- larger-than-one-request write ------------------------------------------
# MAX_DATA in the test daemon is small, so this checks the chunking path
# reports a real errno rather than lying about a short write.
dd if=/dev/zero of=$MNT/big.bin bs=1024 count=1 2>/dev/null
check "oversized write reports failure, not silent truncation" \
	sh -c "test ! -f $MNT/big.bin || test \$(stat -c %s $MNT/big.bin) -lt 1024"

# --- teardown ---------------------------------------------------------------
kill -9 $DSD_PID 2>/dev/null
exec 3>&-
sleep 0.2
umount $MNT
check "unmounted" sh -c '! grep -q " dsfs " /proc/mounts'
rmmod ds_fs
check "rmmod clean" sh -c '! lsmod | grep -q ds_fs'

summary
