# Stage 1: the module on its own, before any daemon exists.
#
# There is exactly one kind of alloyfs mount — one served by a daemon over
# /dev/alloyfs — so the interesting question at this stage is what happens
# when there is nothing behind the mount. A filesystem that answers such a
# request with an empty tree is worse than one that refuses: it looks like it
# worked, and every later assertion is then made against fiction.
#
# The inotify assertions this stage used to carry moved to stage 3, where the
# same events arrive from a real daemon instead of a hardcoded tree.
. /tests/lib.sh

MNT=/mnt/alloyfs

insmod /lib/modules/alloyfs.ko || { echo "  FAIL: insmod"; exit 1; }
check "module loaded"  grep -q alloyfs /proc/filesystems
check "device created" test -c /dev/alloyfs
mkdir -p $MNT

# --- a mount with no daemon behind it ---------------------------------------
check "fd-less mount refused" sh -c "! mount -t alloyfs none $MNT 2>/dev/null"
check "nothing got mounted"   sh -c "! grep -q ' alloyfs ' /proc/mounts"

# fd= has to name one of OUR descriptors. A valid fd pointing at something
# else is the likelier mistake of the two, and reaches a different check in
# the module (the file operations do not match) than a number that is not
# open at all.
exec 4</dev/null
check "fd= naming another file refused" sh -c "! mount -t alloyfs -o fd=4 none $MNT 2>/dev/null"
exec 4<&-
check "fd= naming no file refused"      sh -c "! mount -t alloyfs -o fd=999 none $MNT 2>/dev/null"
check "still nothing mounted"           sh -c "! grep -q ' alloyfs ' /proc/mounts"

# --- a refused mount must not have kept anything ----------------------------
# rmmod is the assertion here: a superblock or connection left behind by a
# failed mount holds a module reference, and this is where that shows up.
rmmod alloyfs
check "rmmod clean" sh -c '! lsmod | grep -q alloyfs'

# And it all still works the second time around.
insmod /lib/modules/alloyfs.ko || { echo "  FAIL: reinsmod"; exit 1; }
check "reloads cleanly" grep -q alloyfs /proc/filesystems
rmmod alloyfs
check "second rmmod clean" sh -c '! lsmod | grep -q alloyfs'

summary
