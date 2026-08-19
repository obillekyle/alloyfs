#!/bin/sh
# Installs the AlloyFS AGENT as a system-wide systemd service.
#
#   curl -fsSL https://alloy.okyle.dev/service.sh | sudo sh
#   curl -fsSL https://alloy.okyle.dev/service.sh | sudo sh -s -- --user alice
#
# Removes with:
#   sudo systemctl disable --now alloyfs@USER
#
# For MOUNTS, or for anything belonging to one account, use `alloyfs service`
# instead. It registers systemd USER units: no root, no template unit, and the
# process runs inside the session that owns the mount — so an ssh:// mount finds
# the right keys and agent. This script covers the case a user unit cannot, an
# agent on a machine nobody logs into.
#
# POSIX sh, no bashisms: this runs under whatever /bin/sh is on the box.
set -eu

SERVICE_USER=""
EXE=""
START=1

while [ $# -gt 0 ]; do
  case "$1" in
  --user) SERVICE_USER="${2:?--user wants a username}"; shift 2 ;;
  --exe) EXE="${2:?--exe wants a path}"; shift 2 ;;
  --no-start) START=0; shift ;;
  -h | --help) sed -n '2,15p' "$0"; exit 0 ;;
  *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done

[ "$(id -u)" -eq 0 ] || {
  echo "error: needs root (systemd units live in /etc/systemd/system)." >&2
  echo "       re-run with sudo." >&2
  exit 1
}
command -v systemctl >/dev/null 2>&1 || {
  echo "error: no systemctl. This box does not use systemd; run 'alloyfs serve'" >&2
  echo "       under whatever supervisor it does use." >&2
  exit 1
}

# Default to whoever invoked sudo, not to root. An agent running as root would
# serve root's config and export files as root, which is not what anyone means
# by "run it as a service".
[ -n "$SERVICE_USER" ] || SERVICE_USER="${SUDO_USER:-}"
[ -n "$SERVICE_USER" ] || {
  echo "error: cannot tell which user to run as; pass --user NAME." >&2
  exit 1
}
id "$SERVICE_USER" >/dev/null 2>&1 || { echo "error: no such user: $SERVICE_USER" >&2; exit 1; }

# Find the binary. install.sh defaults to ~/.local/bin, which systemd will not
# search, so resolve to an absolute path now and bake it into the unit.
if [ -z "$EXE" ]; then
  for candidate in \
    /usr/local/bin/alloyfs \
    /usr/bin/alloyfs \
    "$(getent passwd "$SERVICE_USER" | cut -d: -f6)/.local/bin/alloyfs"; do
    [ -x "$candidate" ] && { EXE="$candidate"; break; }
  done
fi
[ -n "$EXE" ] || {
  echo "error: cannot find the alloyfs binary." >&2
  echo "       install it (curl -fsSL https://alloy.okyle.dev/install.sh | sh)," >&2
  echo "       or pass --exe /path/to/alloyfs." >&2
  exit 1
}
[ -x "$EXE" ] || { echo "error: not executable: $EXE" >&2; exit 1; }

# A binary under a home directory is readable by systemd but is a footgun: it
# vanishes if that user is removed, and on some systems /home is mounted late.
case "$EXE" in
/home/* | /root/*)
  echo "note: $EXE lives in a home directory. Consider moving it to" >&2
  echo "      /usr/local/bin so the unit survives user changes." >&2
  ;;
esac

UNIT=/etc/systemd/system/alloyfs@.service
echo "writing $UNIT (ExecStart=$EXE serve)"

cat >"$UNIT" <<EOF
# systemd template unit for the AlloyFS agent.
# Installed by https://alloy.okyle.dev/service.sh
#
# The instance name is the user whose config and exports it serves:
#   sudo systemctl enable --now alloyfs@$SERVICE_USER
# Logs: journalctl -u alloyfs@$SERVICE_USER -f
#
# The agent only, deliberately. A system unit runs outside the user's session,
# so it has no XDG_RUNTIME_DIR, no session bus and no SSH_AUTH_SOCK — fine for
# serving local folders, useless for an ssh:// mount. Mounts belong to
# `alloyfs service`, which registers a user unit per account.
#
# No --config: the agent finds ~<user>/.alloyfs/config.yml on its own, and
# creates a starter one if none exists. An explicit path would defeat that.

[Unit]
Description=alloyfs agent for %i (virtual drive exports)
After=network-online.target
Wants=network-online.target

[Service]
User=%i
ExecStart=$EXE serve
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info
# The agent only needs to read/write the exported folders.
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload

# Warn before enabling rather than after it fails: an agent with no config
# starts, serves nothing, and looks healthy.
HOME_DIR="$(getent passwd "$SERVICE_USER" | cut -d: -f6)"
if [ ! -f "$HOME_DIR/.alloyfs/config.yml" ] &&
  [ ! -f "$HOME_DIR/.alloyfs/config.yaml" ] &&
  [ ! -f "$HOME_DIR/.alloyfs/config.json" ]; then
  echo "warning: $SERVICE_USER has no ~/.alloyfs/config.yml — the agent will" >&2
  echo "         start and serve nothing. Create one with: alloyfs init --global" >&2
fi

if [ "$START" -eq 1 ]; then
  systemctl enable --now "alloyfs@$SERVICE_USER"
  echo
  echo "alloyfs@$SERVICE_USER is enabled and running."
else
  systemctl enable "alloyfs@$SERVICE_USER"
  echo
  echo "alloyfs@$SERVICE_USER is enabled (not started; --no-start)."
fi

echo "  status:  systemctl status alloyfs@$SERVICE_USER"
echo "  logs:    journalctl -u alloyfs@$SERVICE_USER -f"
echo "  remove:  sudo systemctl disable --now alloyfs@$SERVICE_USER"
