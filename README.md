# mtop

A small REST API (Rust + axum) that reports CPU, memory, swap, and disk usage.
It runs as a hardened systemd service on Ubuntu.

A background thread takes a sample every `MTOP_INTERVAL_SECS` (default 5s).
Requests are answered from the latest sample, so they're cheap. A rolling
history (default 1 hour) is kept in memory.

## Install

**From a release tarball** (no Rust needed on the target):

```sh
./package.sh                         # on a build machine -> dist/mtop-<ver>-<arch>.tar.gz
scp dist/mtop-*.tar.gz host:
ssh host 'tar xzf mtop-*.tar.gz && cd mtop-*/ && sudo ./install.sh'
```

**From source** (on the target; installs build-essential + rustup if missing):

```sh
sudo ./install.sh
```

Options: `--port N` (default 8787), `--bind ADDR` (default 0.0.0.0), `--token T`
(default: a random one is generated), `--interval SECS`, `--history SECS`, `--from-source`.
Re-running `install.sh` upgrades in place and keeps the existing token and config.

Config lives in `/etc/mtop/mtop.env`. After editing it, run `sudo systemctl restart mtop`.
To remove: `sudo ./uninstall.sh [--purge]`.

## API

Every `/api/v1/*` endpoint needs `Authorization: Bearer <token>`.

| Endpoint | Description |
|---|---|
| `GET /health` | Liveness check (no auth) |
| `GET /api/v1/metrics` | Everything: hostname, uptime, cpu, memory, disks |
| `GET /api/v1/cpu` | Overall %, per-core %, load average (1/5/15) |
| `GET /api/v1/memory` | RAM total/used/available/% and swap |
| `GET /api/v1/disks` | Every real mounted filesystem (tmpfs/squashfs/overlay etc. excluded) |
| `GET /api/v1/history?seconds=N` | Compact samples from the last N seconds (default: all) |

```sh
TOKEN=$(sudo sed -n 's/^MTOP_TOKEN=//p' /etc/mtop/mtop.env)
curl -H "Authorization: Bearer $TOKEN" http://localhost:8787/api/v1/metrics
```

Disk `used_bytes` is `total - available`. It counts root-reserved blocks as used,
so it can read a few percent higher than `df`.

## Environment variables

| Variable | Default | |
|---|---|---|
| `MTOP_BIND` | `0.0.0.0:8787` | Listen address |
| `MTOP_TOKEN` | (required) | Bearer token |
| `MTOP_NO_AUTH` | unset | Set to `1` to run without auth (dev only) |
| `MTOP_INTERVAL_SECS` | `5` | Sampling interval |
| `MTOP_HISTORY_SECS` | `3600` | How much history to keep |

Local dev: `MTOP_NO_AUTH=1 cargo run`
