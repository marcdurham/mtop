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

### Cross-compiling for another architecture

`package.sh` defaults to this machine's architecture (`<arch>-unknown-linux-musl`).
To build on a fast machine for a different target, such as an aarch64 board from an
x86_64 PC, pass the Rust target:

```sh
./package.sh aarch64-unknown-linux-musl     # -> dist/mtop-<ver>-aarch64.tar.gz
```

The script adds the rustup target for you, but cargo also needs a linker for it.
Either install a cross gcc (`sudo apt install gcc-aarch64-linux-gnu`) and add to
`.cargo/config.toml`:

```toml
[target.aarch64-unknown-linux-musl]
linker = "aarch64-linux-gnu-gcc"
```

or, with no extra packages, use the `rust-lld` that ships with the toolchain:

```toml
[target.aarch64-unknown-linux-musl]
linker = "rust-lld"
rustflags = ["-C", "link-self-contained=yes"]
```

Then copy and install as above:

```sh
scp dist/mtop-*-aarch64.tar.gz user@host:/tmp/
ssh -t user@host 'cd /tmp && tar xzf mtop-*-aarch64.tar.gz && cd mtop-*/ && sudo ./install.sh'
```

## Running as a service

`install.sh` installs mtop as a systemd service that starts at boot and restarts
automatically if it crashes (`Restart=always`). It runs as an unprivileged `mtop`
user. Paths: binary `/usr/local/bin/mtop`, config `/etc/mtop/mtop.env`, unit
`/etc/systemd/system/mtop.service`.

```sh
systemctl status mtop           # is it running?
journalctl -u mtop -f           # follow logs
sudo systemctl restart mtop     # after editing /etc/mtop/mtop.env
sudo systemctl stop mtop        # stop until next boot / start
sudo systemctl start mtop
sudo systemctl disable --now mtop   # stop and don't start at boot
```

If `ufw` is active, allow remote access with `sudo ufw allow 8787/tcp`.
To upgrade, build a new tarball, copy it over and re-run `sudo ./install.sh`.

## API

Every `/api/v1/*` endpoint needs `Authorization: Bearer <token>`.

| Endpoint | Description |
|---|---|
| `GET /health` | Liveness check (no auth) |
| `GET /api/v1/metrics` | Everything: hostname, uptime, cpu, memory, disks |
| `GET /api/v1/cpu` | Overall %, per-core %, load average (1/5/15) |
| `GET /api/v1/memory` | RAM total/used/available/% and swap |
| `GET /api/v1/disks` | Every real mounted filesystem (tmpfs/squashfs/overlay etc. excluded) |
| `GET /api/v1/proxy` | Domains served by Caddy or nginx on this host, and where each is forwarded to |
| `GET /api/v1/history?seconds=N` | Compact samples from the last N seconds (default: all) |

```sh
TOKEN=$(sudo sed -n 's/^MTOP_TOKEN=//p' /etc/mtop/mtop.env)
curl -H "Authorization: Bearer $TOKEN" http://localhost:8787/api/v1/metrics
```

Disk `used_bytes` is `total - available`. It counts root-reserved blocks as used,
so it can read a few percent higher than `df`.

The service has `CAP_DAC_READ_SEARCH` so it can read the size of drives mounted
where the `mtop` user can't go, like USB drives the desktop automounts under
`/media/<user>` (that directory only lets its owner in). Without it those drives
are silently left out.

## Environment variables

| Variable | Default | |
|---|---|---|
| `MTOP_BIND` | `0.0.0.0:8787` | Listen address |
| `MTOP_TOKEN` | (required) | Bearer token |
| `MTOP_NO_AUTH` | unset | Set to `1` to run without auth (dev only) |
| `MTOP_INTERVAL_SECS` | `5` | Sampling interval |
| `MTOP_HISTORY_SECS` | `3600` | How much history to keep |
| `MTOP_PROXY` | `auto` | Reverse proxy to report domains for: `auto`, `caddy`, `nginx` or `off` |

### Reverse-proxy domains

When Caddy or nginx runs on the host, `/api/v1/metrics` (and `/api/v1/proxy`) include a `proxy`
object: the server, whether it is running, and every domain it serves with its targets — upstream
`host:port`s, or `files` / `redirect` / `respond`. Caddy is read from its admin API
(`localhost:2019/config/`), falling back to `/etc/caddy/Caddyfile`; nginx from
`/etc/nginx/nginx.conf` with its `include`s followed (`server_name` plus `proxy_pass` and the other
`*_pass` directives, with `upstream` blocks resolved). The config is re-read once a minute.
`/api/v1/history` points carry `proxy_domains`, the count.

Local dev: `MTOP_NO_AUTH=1 cargo run`
