#!/usr/bin/env bash
# Remove everything packaging/install.sh put on this machine: the agent
# binary, the systemd template unit (and every enabled instance of it), the
# DKMS module, the udev rule, the boot-time module load, and the `alloyfs`
# group.
#
#   sudo packaging/uninstall.sh
#
# What it deliberately does NOT touch: ~/.alloyfs — config, the durable data
# tree with the overlay in it, and the cache. Uninstalling software should
# never be how you lose files that exist on no server. Remove those yourself
# if you really mean to.
#
# Idempotent: running it on a machine that has nothing installed reports
# "nothing to remove" and exits 0, so it is safe in scripts and safe to run
# twice after a partial install.
set -euo pipefail

PREFIX="/usr/local"
KEEP_GROUP=0

usage() {
	cat <<'EOF'
usage: sudo packaging/uninstall.sh [options]

  --prefix DIR     where the binary was installed (default /usr/local)
  --keep-group     leave the `alloyfs` group in place
  -h, --help       this text

Leaves ~/.alloyfs (config, data, cache) alone, always.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--prefix) PREFIX="$2"; shift 2 ;;
	--keep-group) KEEP_GROUP=1; shift ;;
	-h|--help) usage; exit 0 ;;
	*) echo "uninstall.sh: unknown option: $1" >&2; usage >&2; exit 2 ;;
	esac
done

say()  { printf '  %s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
die()  { printf 'uninstall.sh: %s\n' "$*" >&2; exit 1; }

REMOVED=0
note() { REMOVED=$((REMOVED + 1)); say "$@"; }

[ "$(uname -s)" = "Linux" ] || die "this uninstaller is Linux-only (uname says $(uname -s))."
if [ "$(id -u)" -ne 0 ]; then
	die "needs root to remove files from $PREFIX/bin, /etc and /usr/src.
Re-run it as:  sudo $0 $*"
fi

# ------------------------------------------------------------------ service
# Instances first: stopping the units before removing the binary means no
# instance is ever left running against a binary that no longer exists, and
# no enabled-but-broken symlink survives in multi-user.target.wants.
step "systemd"
if command -v systemctl >/dev/null 2>&1; then
	# Both running and merely-enabled instances, deduplicated. --no-legend
	# --plain keeps the output parseable across systemd versions.
	INSTANCES="$( {
		systemctl list-units --type=service --all --no-legend --plain 'alloyfs@*.service' 2>/dev/null | awk '{print $1}'
		systemctl list-unit-files --no-legend --plain 'alloyfs@*.service' 2>/dev/null | awk '{print $1}'
	} | grep -E '^alloyfs@.+\.service$' | sort -u || true)"
	for unit in $INSTANCES; do
		systemctl disable --now "$unit" >/dev/null 2>&1 || true
		note "stopped and disabled $unit"
	done
	if [ -e /etc/systemd/system/alloyfs@.service ]; then
		rm -f /etc/systemd/system/alloyfs@.service
		note "removed /etc/systemd/system/alloyfs@.service"
	fi
	# Sweeps up any dangling instance symlinks left by a hand-rolled enable.
	find /etc/systemd/system -name 'alloyfs@*.service' -type l -delete 2>/dev/null || true
	systemctl daemon-reload
	systemctl reset-failed 'alloyfs@*.service' >/dev/null 2>&1 || true
else
	say "no systemctl here; skipping"
fi

# ------------------------------------------------------------ kernel module
step "kernel module"
if [ -e /etc/modules-load.d/alloyfs.conf ]; then
	rm -f /etc/modules-load.d/alloyfs.conf
	note "removed /etc/modules-load.d/alloyfs.conf"
fi

# Unload before deregistering, so the device node goes away with the module
# rather than lingering as a node backed by nothing.
if lsmod 2>/dev/null | grep -q '^alloyfs '; then
	if modprobe -r alloyfs 2>/dev/null; then
		note "unloaded the alloyfs module"
	else
		say "WARNING: the alloyfs module is in use and could not be unloaded."
		say "         Unmount every alloyfs filesystem and stop anything holding"
		say "         /dev/alloyfs open, then \`modprobe -r alloyfs\`. The on-disk"
		say "         files are removed below either way, so a reboot also clears it."
	fi
fi

if command -v dkms >/dev/null 2>&1; then
	# Non-interactive for the same reason install.sh is: on a Secure Boot
	# machine dkms can drop into a debconf password prompt that loops forever
	# without a terminal. (We never touch the enrolled MOK itself — it belongs
	# to DKMS and is shared with every other out-of-tree module on the box.)
	export DEBIAN_FRONTEND=noninteractive
	# Every registered version, not just the one matching this checkout: an
	# upgrade that changed the workspace version would otherwise strand the
	# old one, still auto-rebuilding on every kernel upgrade forever.
	#
	# Versions come from the DKMS state tree rather than from parsing
	# `dkms status`, whose output format differs between DKMS 2.x
	# ("alloyfs, 0.1.0, kernel, arch: installed") and 3.x
	# ("alloyfs/0.1.0, kernel, arch: installed"). Directory names are the
	# same in both and cannot be misparsed.
	for verdir in /var/lib/dkms/alloyfs/*/; do
		[ -d "$verdir" ] || continue
		entry="$(basename "$verdir")"
		# Skip the kernel-<ver>-<arch> convenience symlinks alongside them.
		case "$entry" in kernel-*) continue ;; esac
		dkms remove -m alloyfs -v "$entry" --all >/dev/null 2>&1 || true
		note "deregistered alloyfs/$entry from DKMS"
	done
fi
# DKMS leaves this behind if it was interrupted, or if its own state got out
# of step with /usr/src. A stale /var/lib/dkms/alloyfs/<ver>/source dangling
# at a removed directory makes plain `dkms status` fail for EVERY module on
# the machine, so this sweep is not cosmetic.
if [ -d /var/lib/dkms/alloyfs ]; then
	rm -rf /var/lib/dkms/alloyfs
	note "removed /var/lib/dkms/alloyfs"
fi
for src in /usr/src/alloyfs-*; do
	[ -d "$src" ] || continue
	rm -rf "$src"
	note "removed $src"
done
# DKMS normally takes its installed .ko with it; belt and braces for the case
# where dkms itself has already been uninstalled from the machine.
for ko in /lib/modules/*/updates/dkms/alloyfs.ko*; do
	[ -e "$ko" ] || continue
	rm -f "$ko"
	note "removed $ko"
done
depmod -a 2>/dev/null || true

if [ -e /etc/udev/rules.d/60-alloyfs.rules ]; then
	rm -f /etc/udev/rules.d/60-alloyfs.rules
	udevadm control --reload-rules 2>/dev/null || true
	note "removed /etc/udev/rules.d/60-alloyfs.rules"
fi

# Only ever a stale node from a hard-killed module; with the module unloaded
# there is nothing behind it, and leaving it would let a later unrelated misc
# minor be reached through a name that promises alloyfs.
if [ -e /dev/alloyfs ] && ! lsmod 2>/dev/null | grep -q '^alloyfs '; then
	rm -f /dev/alloyfs
	note "removed the stale /dev/alloyfs node"
fi

if [ "$KEEP_GROUP" -eq 0 ] && getent group alloyfs >/dev/null; then
	groupdel alloyfs 2>/dev/null &&
		note "removed the alloyfs group" ||
		say "could not remove the alloyfs group (it is some user's primary group?)"
fi

# ------------------------------------------------------------------- binary
step "agent binary"
if [ -e "$PREFIX/bin/alloyfs" ]; then
	rm -f "$PREFIX/bin/alloyfs"
	note "removed $PREFIX/bin/alloyfs"
fi

step "done"
if [ "$REMOVED" -eq 0 ]; then
	say "nothing to remove — AlloyFS was not installed under $PREFIX"
else
	say "$REMOVED item(s) removed"
fi
say "left alone: ~/.alloyfs (config, data/, cache/) for every user"
