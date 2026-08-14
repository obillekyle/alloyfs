# Stage 0: prove the harness itself works before any kernel code exists.
# If this can't pass, nothing built on top of it can be trusted.
. /tests/lib.sh

check "proc mounted"     test -r /proc/version
check "sys mounted"      test -d /sys/kernel
check "devtmpfs mounted" test -c /dev/null
check "tmp writable"     sh -c 'mkdir -p /tmp && echo x > /tmp/probe && test -s /tmp/probe'

kver=$(cat /proc/version | cut -d' ' -f3)
echo "  kernel: $kver"
check "inotify available" test -d /proc/sys/fs/inotify

# The probe is the instrument every later stage measures with — verify it
# against a filesystem the kernel definitely notifies on (tmpfs), so a
# failure here means a broken probe, not a broken driver.
mkdir -p /tmp/w
ds-inotify -t 3 /tmp/w > /tmp/ev.log 2>&1 &
probe=$!
# Wait for READY so we can't race the watch being added.
tries=0
while ! grep -q '^READY' /tmp/ev.log 2>/dev/null; do
	tries=$((tries + 1))
	[ "$tries" -gt 100 ] && break
	sleep 0.05
done
check "probe reached READY" grep -q '^READY' /tmp/ev.log

echo hello > /tmp/w/a.txt
mv /tmp/w/a.txt /tmp/w/b.txt
rm /tmp/w/b.txt
wait $probe

echo "  --- probe transcript ---"
cat /tmp/ev.log
echo "  ------------------------"

check "tmpfs CREATE seen"     grep -q '^EV .* 00000100 0 a.txt' /tmp/ev.log
check "tmpfs MOVED_FROM seen" grep -q '^EV .* 00000040 ' /tmp/ev.log
check "tmpfs MOVED_TO seen"   grep -q '^EV .* 00000080 ' /tmp/ev.log
check "tmpfs DELETE seen"     grep -q '^EV .* 00000200 0 b.txt' /tmp/ev.log

# The cookie pairing is THE assertion the whole project rests on; if the probe
# can't observe it on tmpfs, it can't judge our driver either.
cf=$(grep '^EV .* 00000040 ' /tmp/ev.log | head -1 | cut -d' ' -f4)
ct=$(grep '^EV .* 00000080 ' /tmp/ev.log | head -1 | cut -d' ' -f4)
eq "rename cookies match" "$cf" "$ct"
check "rename cookie nonzero" test "$cf" != "0"

summary
