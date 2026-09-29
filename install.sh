#!/usr/bin/env bash
# Install mtop as a systemd service on Ubuntu.
#
#   sudo ./install.sh [--port N] [--bind ADDR] [--token TOKEN] [--interval SECS]
#                     [--history SECS] [--from-source]
#
# If an `mtop` binary sits next to this script (e.g. from a release tarball
# made by package.sh) it is installed directly. Otherwise mtop is built from
# source, installing build-essential and rustup if needed.
#
# Re-running upgrades the binary and keeps the existing token/config unless
# options are passed to override them.
set -euo pipefail

SERVICE=mtop
BIN_PATH=/usr/local/bin/mtop
CONF_DIR=/etc/mtop
ENV_FILE=$CONF_DIR/mtop.env
UNIT_FILE=/etc/systemd/system/$SERVICE.service
SERVICE_USER=mtop

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PORT=""
BIND_ADDR=""
TOKEN=""
INTERVAL=""
HISTORY=""
FROM_SOURCE=0

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

usage() { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port)        PORT="${2:?}"; shift 2 ;;
        --bind)        BIND_ADDR="${2:?}"; shift 2 ;;
        --token)       TOKEN="${2:?}"; shift 2 ;;
        --interval)    INTERVAL="${2:?}"; shift 2 ;;
        --history)     HISTORY="${2:?}"; shift 2 ;;
        --from-source) FROM_SOURCE=1; shift ;;
        -h|--help)     usage 0 ;;
        *)             warn "unknown option: $1"; usage 1 ;;
    esac
done

[[ $EUID -eq 0 ]] || die "run as root: sudo $0 $*"
command -v systemctl >/dev/null || die "systemd is required"

if [[ -n "$PORT" && ! "$PORT" =~ ^[0-9]+$ ]]; then die "--port must be a number"; fi

# ---------------------------------------------------------------------------
# 1. Obtain the binary
# ---------------------------------------------------------------------------
build_from_source() {
    [[ -f "$SCRIPT_DIR/Cargo.toml" ]] || die "no prebuilt binary and no Cargo.toml in $SCRIPT_DIR"

    # Build as the invoking (non-root) user so rustup/cargo live in their home
    # and target/ isn't root-owned.
    local build_user="${SUDO_USER:-root}"
    local build_home
    build_home="$(getent passwd "$build_user" | cut -d: -f6)"
    as_user() { sudo -u "$build_user" -H env PATH="$build_home/.cargo/bin:$PATH" "$@"; }

    if ! dpkg -s build-essential >/dev/null 2>&1 || ! command -v curl >/dev/null; then
        log "Installing build dependencies (build-essential, curl)"
        apt-get update -qq
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential curl ca-certificates
    fi

    if ! as_user bash -c 'command -v cargo' >/dev/null 2>&1; then
        log "Installing Rust toolchain via rustup for user '$build_user'"
        as_user bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
    fi

    log "Building mtop (release) as '$build_user' — this can take a minute"
    as_user bash -c "cd '$SCRIPT_DIR' && cargo build --release"
    BIN_SRC="$SCRIPT_DIR/target/release/mtop"
}

if [[ $FROM_SOURCE -eq 0 && -x "$SCRIPT_DIR/mtop" && -f "$SCRIPT_DIR/mtop" ]]; then
    BIN_SRC="$SCRIPT_DIR/mtop"
    log "Using prebuilt binary: $BIN_SRC"
else
    build_from_source
fi

file_out="$(file -b "$BIN_SRC" 2>/dev/null || true)"
case "$(uname -m)" in
    x86_64)  [[ -z "$file_out" || "$file_out" == *x86-64* ]] || die "binary arch mismatch: $file_out" ;;
    aarch64) [[ -z "$file_out" || "$file_out" == *aarch64* ]] || die "binary arch mismatch: $file_out" ;;
esac

# ---------------------------------------------------------------------------
# 2. Service user
# ---------------------------------------------------------------------------
if ! id "$SERVICE_USER" >/dev/null 2>&1; then
    log "Creating system user '$SERVICE_USER'"
    useradd --system --no-create-home --home-dir /nonexistent \
            --shell /usr/sbin/nologin "$SERVICE_USER"
fi

# ---------------------------------------------------------------------------
# 3. Binary
# ---------------------------------------------------------------------------
if systemctl is-active --quiet "$SERVICE"; then
    log "Stopping running $SERVICE for upgrade"
    systemctl stop "$SERVICE"
fi
log "Installing binary to $BIN_PATH"
install -m 0755 -o root -g root "$BIN_SRC" "$BIN_PATH"

# ---------------------------------------------------------------------------
# 4. Config (preserve existing values on reinstall)
# ---------------------------------------------------------------------------
install -d -m 0750 -o root -g "$SERVICE_USER" "$CONF_DIR"

existing() { [[ -f "$ENV_FILE" ]] && sed -n "s/^$1=//p" "$ENV_FILE" | tail -n1 || true; }

cur_bind="$(existing MTOP_BIND)"
cur_bind="${cur_bind:-0.0.0.0:8787}"
if [[ -n "$BIND_ADDR" ]]; then
    cur_bind="$BIND_ADDR:${PORT:-${cur_bind##*:}}"
elif [[ -n "$PORT" ]]; then
    cur_bind="${cur_bind%:*}:$PORT"
fi

cur_token="${TOKEN:-$(existing MTOP_TOKEN)}"
TOKEN_GENERATED=0
if [[ -z "$cur_token" ]]; then
    cur_token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    TOKEN_GENERATED=1
fi

cur_interval="${INTERVAL:-$(existing MTOP_INTERVAL_SECS)}"
cur_history="${HISTORY:-$(existing MTOP_HISTORY_SECS)}"

log "Writing $ENV_FILE"
umask 027
cat > "$ENV_FILE" <<EOF
# mtop configuration — restart after editing: sudo systemctl restart mtop
MTOP_BIND=$cur_bind
MTOP_TOKEN=$cur_token
MTOP_INTERVAL_SECS=${cur_interval:-5}
MTOP_HISTORY_SECS=${cur_history:-3600}
EOF
umask 022
chown root:"$SERVICE_USER" "$ENV_FILE"
chmod 0640 "$ENV_FILE"

# ---------------------------------------------------------------------------
# 5. systemd unit
# ---------------------------------------------------------------------------
log "Writing $UNIT_FILE"
cat > "$UNIT_FILE" <<EOF
[Unit]
Description=mtop system metrics REST API
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_USER
EnvironmentFile=$ENV_FILE
ExecStart=$BIN_PATH
Restart=always
RestartSec=3

# Hardening. Filesystems stay visible (read-only) so disk usage can be read.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only
ProtectKernelModules=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
CapabilityBoundingSet=
AmbientCapabilities=
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
SystemCallArchitectures=native

[Install]
WantedBy=multi-user.target
EOF

# Binding a port < 1024 as an unprivileged user needs this capability.
port_num="${cur_bind##*:}"
if (( port_num < 1024 )); then
    sed -i 's/^CapabilityBoundingSet=$/CapabilityBoundingSet=CAP_NET_BIND_SERVICE/; s/^AmbientCapabilities=$/AmbientCapabilities=CAP_NET_BIND_SERVICE/' "$UNIT_FILE"
fi

systemctl daemon-reload
systemctl enable "$SERVICE" >/dev/null 2>&1
log "Starting $SERVICE"
systemctl restart "$SERVICE"

# ---------------------------------------------------------------------------
# 6. Verify
# ---------------------------------------------------------------------------
ok=0
for _ in $(seq 1 20); do
    if curl -fsS "http://127.0.0.1:$port_num/health" >/dev/null 2>&1; then ok=1; break; fi
    sleep 0.5
done
if [[ $ok -ne 1 ]]; then
    systemctl --no-pager status "$SERVICE" || true
    journalctl -u "$SERVICE" -n 20 --no-pager || true
    die "$SERVICE did not become healthy"
fi

if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q "Status: active"; then
    warn "ufw is active; to allow remote access run:  sudo ufw allow $port_num/tcp"
fi

host_ip="$(hostname -I 2>/dev/null | awk '{print $1}')"
echo
log "mtop is running and enabled at boot (listening on $cur_bind)"
if [[ $TOKEN_GENERATED -eq 1 ]]; then
    echo "   Generated API token (also stored in $ENV_FILE):"
else
    echo "   API token (from $ENV_FILE):"
fi
echo "     $cur_token"
echo
echo "   Try it:"
echo "     curl -H 'Authorization: Bearer $cur_token' http://${host_ip:-127.0.0.1}:$port_num/api/v1/metrics"
echo
echo "   Manage:  systemctl status $SERVICE | journalctl -u $SERVICE -f"
