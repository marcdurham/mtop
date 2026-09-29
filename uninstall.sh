#!/usr/bin/env bash
# Remove the mtop service. Pass --purge to also delete /etc/mtop and the mtop user.
set -euo pipefail

[[ $EUID -eq 0 ]] || { echo "run as root: sudo $0 $*" >&2; exit 1; }

systemctl disable --now mtop 2>/dev/null || true
rm -f /etc/systemd/system/mtop.service /usr/local/bin/mtop
systemctl daemon-reload

if [[ "${1:-}" == "--purge" ]]; then
    rm -rf /etc/mtop
    id mtop >/dev/null 2>&1 && userdel mtop
    echo "mtop removed (config and user purged)"
else
    echo "mtop removed (config kept in /etc/mtop; use --purge to delete it)"
fi
