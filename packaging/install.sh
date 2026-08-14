#!/usr/bin/env bash
# Install AlloyFS on a Linux machine: the agent binary, the systemd template
# unit, and — optionally — the kernel module via DKMS plus the udev rule that
# makes /dev/alloyfs usable by an unprivileged agent.
#
#   sudo packaging/install.sh                 # binary + unit, no kernel module
#   sudo packaging/install.sh --with-module   # ... and the DKMS module
#   sudo packaging/install.sh --with-module --user alice --start
#
# Everything here is idempotent: running it twice is the supported way to
# upgrade, and it never destroys configuration under ~/.alloyfs.
#
# Build the binary FIRST, as yourself:  cargo build --release
# This script deliberately does not run cargo — doing that under sudo leaves a
# root-owned target/ directory that then breaks every later developer build.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Kept for the "re-run me under sudo" hint, which is only useful if it echoes
# back the options you actually typed. Parsing shifts them away.
ARGV=("$@")

PREFIX="/usr/local"
BINARY=""
WITH_MODULE=0
WITH_SERVICE=1
SERVICE_USER=""
START=0
AUTOLOAD=1

usage() {
	cat <<'EOF'
usage: sudo packaging/install.sh [options]

  --prefix DIR     install the binary under DIR/bin (default /usr/local)
  --binary PATH    use this alloyfs binary instead of target/release/alloyfs
  --with-module    build and install the kernel module through DKMS, create
                   the `alloyfs` group and install the udev rule for
                   /dev/alloyfs (Linux-only, optional; FUSE is the default
                   mount backend and needs none of it)
  --no-autoload    with --with-module, do NOT load the module at boot
  --no-service     do not install the systemd template unit
  --user NAME      unit instance to enable (alloyfs@NAME); with --with-module
                   also adds NAME to the `alloyfs` group
  --start          also start alloyfs@NAME now (implies --user)
  -h, --help       this text

Build the binary first, as your normal user:  cargo build --release
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--prefix) PREFIX="$2"; shift 2 ;;
	--binary) BINARY="$2"; shift 2 ;;
	--with-module) WITH_MODULE=1; shift ;;
	--no-autoload) AUTOLOAD=0; shift ;;
	--no-service) WITH_SERVICE=0; shift ;;
	--user) SERVICE_USER="$2"; shift 2 ;;
	--start) START=1; shift ;;
	-h|--help) usage; exit 0 ;;
	*) echo "install.sh: unknown option: $1" >&2; usage >&2; exit 2 ;;
	esac
done

say()  { printf '  %s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
die()  { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- preflight
# Fail on the FIRST missing prerequisite, before touching anything, so a
# half-installed system is not a state this script can produce.

[ "$(uname -s)" = "Linux" ] || die "this installer is Linux-only (uname says $(uname -s)).
On Windows use scripts/install-agent-task.ps1; on macOS just put the binary on PATH."

if [ "$(id -u)" -ne 0 ]; then
	die "needs root to write $PREFIX/bin, /etc/systemd/system and /usr/src.
Re-run it as:  sudo $0 ${ARGV[*]:-}"
fi

if [ -z "$BINARY" ]; then
	BINARY="$ROOT/target/release/alloyfs"
fi
[ -x "$BINARY" ] || die "no alloyfs binary at $BINARY.
Build one as your normal user first:  cargo build --release
(or point at an existing one with --binary PATH)"

# The workspace version is the single source of truth: it names the DKMS
# package, so `dkms status` and `alloyfs --version` agree.
#
# The \r strip is not paranoia. This repo is developed on Windows with
# `* text=auto`, so Cargo.toml in a working tree copied straight to a Linux
# box has CRLF line endings, and the version comes out as "0.1.0\r". That
# produced a real /usr/src/alloyfs-0.1.0<CR> directory which no ordinary
# shell command could name, and — because GNU make treats \r as whitespace —
# made kbuild read M=<dir>/build as two paths and refuse with the
# spectacularly unhelpful "building multiple external modules is not
# supported". Strip it at the source and validate the shape.
VERSION="$(awk '
	/^\[workspace\.package\]/ { in_sec = 1; next }
	/^\[/                     { in_sec = 0 }
	in_sec && /^version[[:space:]]*=/ { gsub(/[",\r]/, "", $3); print $3; exit }
' "$ROOT/Cargo.toml" | tr -d '\r')"
case "$VERSION" in
	'' | *[!0-9.a-zA-Z+-]*)
		die "could not read a sane workspace version out of $ROOT/Cargo.toml (got '$VERSION')" ;;
esac

if [ "$START" -eq 1 ] && [ -z "$SERVICE_USER" ]; then
	die "--start needs --user NAME (the unit is a template: alloyfs@NAME)"
fi
if [ -n "$SERVICE_USER" ] && ! id "$SERVICE_USER" >/dev/null 2>&1; then
	die "no such user: $SERVICE_USER"
fi
if [ -n "$SERVICE_USER" ] && [ "$WITH_SERVICE" -eq 0 ]; then
	die "--user and --no-service contradict each other"
fi

if [ "$WITH_MODULE" -eq 1 ]; then
	command -v dkms >/dev/null 2>&1 || die "--with-module needs dkms.
  Debian/Ubuntu:  apt install dkms
  Fedora:         dnf install dkms
Or drop --with-module: the FUSE backend is the default and needs no module."
	KDIR="/lib/modules/$(uname -r)/build"
	[ -d "$KDIR" ] || die "--with-module needs the headers for the running kernel.
  $KDIR does not exist.
  Debian/Ubuntu:  apt install linux-headers-$(uname -r)"
fi

# Secure Boot: the kernel refuses modules it cannot verify, and an out-of-tree
# module can only be verified by a machine-owner key (MOK) that a HUMAN
# enrolled from the firmware's MOK Manager screen at boot. That is deliberate
# — the whole point of Secure Boot is that no amount of root access can enroll
# a key behind the machine owner's back — so an installer cannot do it, and
# should not pretend otherwise. We detect the situation up front and hand back
# instructions instead.
sig_enforced() { [ "$(cat /sys/module/module/parameters/sig_enforce 2>/dev/null)" = "Y" ]; }

echo "AlloyFS $VERSION"
say "binary:  $BINARY"
say "prefix:  $PREFIX"

# ------------------------------------------------------------------- binary
step "agent binary"
install -d -m 0755 "$PREFIX/bin"
# install(1) to a busy path replaces the inode rather than writing through it,
# so a running agent keeps its old image and dies only when restarted — which
# is what we want, since we may be about to restart it anyway.
install -m 0755 "$BINARY" "$PREFIX/bin/alloyfs"
say "installed $PREFIX/bin/alloyfs ($("$PREFIX/bin/alloyfs" --version 2>/dev/null || echo '?'))"

# ------------------------------------------------------------ kernel module
if [ "$WITH_MODULE" -eq 1 ]; then
	step "kernel module (DKMS)"

	# The group gates access to /dev/alloyfs; see 60-alloyfs.rules for why a
	# dedicated group and not world-writable. -f makes this a no-op if it
	# already exists, so re-running never disturbs existing membership.
	groupadd -f -r alloyfs
	say "group alloyfs: gid $(getent group alloyfs | cut -d: -f3)"

	SRCDIR="/usr/src/alloyfs-$VERSION"
	# Drop any previous registration of this exact version first, otherwise
	# `dkms add` refuses and a re-run of the installer cannot upgrade a
	# development version in place.
	if dkms status -m alloyfs -v "$VERSION" 2>/dev/null | grep -q .; then
		say "removing the previously registered alloyfs/$VERSION"
		dkms remove -m alloyfs -v "$VERSION" --all >/dev/null 2>&1 || true
	fi
	rm -rf "$SRCDIR"
	install -d -m 0755 "$SRCDIR"
	cp -R "$ROOT/kernel/alloyfs/." "$SRCDIR/"
	# Not `cp -a`: that preserves ownership from the checkout, which for a
	# normal `sudo ./packaging/install.sh` out of a home directory leaves
	# /usr/src/alloyfs-<ver> owned by an unprivileged user. That user could
	# then edit the C sources that DKMS compiles as root and loads into the
	# kernel on the next upgrade — a local privilege escalation handed over by
	# the installer. Root owns everything under /usr/src, always.
	chown -R root:root "$SRCDIR"
	chmod -R u=rwX,go=rX "$SRCDIR"
	# A developer tree usually has objects from a local `make` in it; DKMS
	# copies this directory verbatim into its build tree, and stale objects
	# built against a different kernel are exactly how you get a confusing
	# "invalid module format" later.
	find "$SRCDIR" \( -name '*.o' -o -name '*.ko' -o -name '*.mod' \
		-o -name '*.mod.c' -o -name '.*.cmd' -o -name 'Module.symvers' \
		-o -name 'modules.order' -o -name '.cache.mk' \) -delete
	rm -rf "$SRCDIR/.tmp_versions"

	# Version substituted so the DKMS package and the binary carry the same
	# number. CR stripped for the same reason as everywhere else in this
	# script: a Windows checkout copied to a Linux box.
	sed -e 's/\r$//' -e "s/^PACKAGE_VERSION=.*/PACKAGE_VERSION=\"$VERSION\"/" \
		"$ROOT/packaging/dkms.conf" >"$SRCDIR/dkms.conf"
	chmod 0644 "$SRCDIR/dkms.conf"
	say "sources at $SRCDIR"

	# Every dkms call is non-interactive with stdin closed. On a Secure Boot
	# machine dkms wants to enroll a MOK and asks for the enrollment password
	# through debconf; with no terminal that prompt does not fail, it REPEATS —
	# an unattended install.sh spins forever writing "Invalid password" until
	# something kills it. Learned the hard way.
	export DEBIAN_FRONTEND=noninteractive
	dkms add -m alloyfs -v "$VERSION" </dev/null
	dkms build -m alloyfs -v "$VERSION" </dev/null
	dkms install -m alloyfs -v "$VERSION" --force </dev/null
	say "$(dkms status -m alloyfs -v "$VERSION" | tr '\n' ' ')"

	step "udev rule for /dev/alloyfs"
	# sed, not install(1): udev does not tolerate CRLF the way systemd does —
	# a trailing ^M lands inside the MODE= value and the rule silently
	# mis-parses, which is the worst possible failure for a permission rule.
	sed 's/\r$//' "$ROOT/packaging/60-alloyfs.rules" >/etc/udev/rules.d/60-alloyfs.rules
	chmod 0644 /etc/udev/rules.d/60-alloyfs.rules
	udevadm control --reload-rules
	say "installed /etc/udev/rules.d/60-alloyfs.rules"

	if [ "$AUTOLOAD" -eq 1 ]; then
		# The device node only exists while the module is loaded, and nothing
		# demand-loads it: `mount -t alloyfs` autoloads by filesystem alias,
		# but the daemon has to open /dev/alloyfs *before* there is anything to
		# mount. So the module must be loaded at boot for the kernel backend to
		# be usable at all.
		printf '# Loaded so /dev/alloyfs exists for the AlloyFS agent.\nalloyfs\n' \
			>/etc/modules-load.d/alloyfs.conf
		chmod 0644 /etc/modules-load.d/alloyfs.conf
		say "installed /etc/modules-load.d/alloyfs.conf (module loads at boot)"
	fi

	# Load now so /dev/alloyfs is there without a reboot. A running older
	# build has to go first; if it is busy we say so rather than pretending.
	if lsmod | grep -q '^alloyfs '; then
		if modprobe -r alloyfs 2>/dev/null; then
			say "unloaded the previously running alloyfs module"
		else
			say "WARNING: the running alloyfs module is busy and was not replaced."
			say "         Unmount every alloyfs filesystem and \`modprobe -r alloyfs\`,"
			say "         or reboot, to start using the newly installed one."
		fi
	fi
	MOK_PENDING=0
	if ! lsmod | grep -q '^alloyfs '; then
		if MODPROBE_ERR="$(modprobe alloyfs 2>&1)"; then
			say "loaded the alloyfs module"
		elif sig_enforced; then
			# Expected, not a failure: everything is correctly installed and
			# will load the moment the key is trusted. Bailing out here would
			# leave the machine half-installed for no reason.
			MOK_PENDING=1
			MOK_DER="/var/lib/shim-signed/mok/MOK.der"
			[ -e "$MOK_DER" ] || MOK_DER="the DKMS machine-owner key"
			say "the module is built and installed, but this kernel would not load it:"
			say "    ${MODPROBE_ERR}"
			say ""
			say "  Secure Boot is enforcing module signatures here, and the key DKMS"
			say "  signs with is not enrolled yet. Enrolling it is a one-time,"
			say "  deliberately manual step:"
			say "      sudo mokutil --import $MOK_DER"
			say "      # choose a password, reboot, and pick 'Enroll MOK' in the"
			say "      # blue MOK Manager screen the firmware shows on the way up"
			say "  After that this module — and every future rebuild of it — loads"
			say "  normally. Until then /dev/alloyfs does not exist and the kernel"
			say "  mount backend is unavailable; FUSE mounts are entirely unaffected."
		else
			die "the module installed but would not load: $MODPROBE_ERR
Check dmesg for detail."
		fi
	fi
	if [ "$MOK_PENDING" -eq 0 ]; then
		# udev applies the rule when the device appears; if the module was
		# already loaded before the rule landed, nudge it.
		udevadm trigger --subsystem-match=misc --action=change >/dev/null 2>&1 || true
		udevadm settle --timeout=10 >/dev/null 2>&1 || true
		if [ -e /dev/alloyfs ]; then
			say "$(ls -l /dev/alloyfs)"
		else
			say "WARNING: /dev/alloyfs did not appear; check dmesg"
		fi
	fi

	if [ -n "$SERVICE_USER" ]; then
		usermod -aG alloyfs "$SERVICE_USER"
		say "added $SERVICE_USER to the alloyfs group (re-login for it to take effect)"
	fi
fi

# ------------------------------------------------------------------ service
if [ "$WITH_SERVICE" -eq 1 ]; then
	step "systemd template unit"
	# Two rewrites on the way in:
	#  - drop CR: a checkout made on Windows (this repo's other half) carries
	#    CRLF, and while systemd tolerates it, a unit file full of ^M is not
	#    something to leave in /etc for the next person to debug;
	#  - ExecStart in the shipped unit points at /usr/local/bin, so rewrite it
	#    when installing anywhere else — otherwise --prefix is a quiet lie.
	sed -e 's/\r$//' \
		-e "s|^ExecStart=/usr/local/bin/alloyfs|ExecStart=$PREFIX/bin/alloyfs|" \
		"$ROOT/scripts/alloyfs.service" >/etc/systemd/system/alloyfs@.service
	chmod 0644 /etc/systemd/system/alloyfs@.service
	systemctl daemon-reload
	say "installed /etc/systemd/system/alloyfs@.service"

	if [ -n "$SERVICE_USER" ]; then
		systemctl enable "alloyfs@$SERVICE_USER" >/dev/null
		say "enabled alloyfs@$SERVICE_USER"
		if [ "$START" -eq 1 ]; then
			systemctl restart "alloyfs@$SERVICE_USER"
			say "started alloyfs@$SERVICE_USER: $(systemctl is-active "alloyfs@$SERVICE_USER")"
		fi
	else
		say "enable it for a user with:  systemctl enable --now alloyfs@\$USER"
	fi
fi

step "done"
say "config lives in ~/.alloyfs/config.yml (created on first run)"
say "uninstall with:  sudo packaging/uninstall.sh"
