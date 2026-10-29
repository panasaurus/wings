# Installation Guide — Panasaurus Wings v0.2.1

Panasaurus Wings is a high-performance, secure server control daemon for
game servers. It communicates with a Panasaurus (or Pterodactyl-compatible)
panel over HTTP, manages Docker containers for game servers, and exposes
SFTP + an SSH CLI for end users.

This guide covers installing Wings from source or from a pre-built binary,
configuring it, registering it with the panel, and running it as a systemd
service.

---

## Table of Contents

1. [Requirements](#1-requirements)
2. [Quick Start](#2-quick-start)
3. [Install from a Pre-built Binary](#3-install-from-a-pre-built-binary)
4. [Build from Source](#4-build-from-source)
5. [Configuration](#5-configuration)
6. [Register the Node with the Panel](#6-register-the-node-with-the-panel)
7. [Run as a systemd Service](#7-run-as-a-systemd-service)
8. [Verify the Installation](#8-verify-the-installation)
9. [Upgrading](#9-upgrading)
10. [Uninstalling](#10-uninstalling)

---

## 1. Requirements

| Component | Minimum | Recommended |
|-----------|---------|-------------|
| OS | Linux x86_64 (kernel 4.15+) | Ubuntu 22.04 / Debian 12 / Rocky 9 |
| RAM | 1 GB | 2 GB+ |
| Disk | 10 GB | SSD, sized to total server data |
| Docker | 20.10+ | 24.x with `compose` plugin |
| Rust | 1.85+ (build only) | Latest stable |
| Ports | 8081/tcp (API), 2022/tcp (SFTP) | Behind a reverse proxy for TLS |

Wings must run as **root** (or a user with full Docker access) so it can
manage containers and set filesystem permissions. The daemon drops
privileges inside containers via the `docker.user` setting.

**Network**: The panel must be able to reach the Wings API port
(`api.port`, default 8081), and Wings must be able to reach the panel URL
(`remote`). For production, put Wings behind a TLS-terminating reverse
proxy (nginx, Caddy) — the plaintext API only allows loopback binds.

---

## 2. Quick Start

```bash
# 1. Install Docker if you haven't already.
curl -fsSL https://get.docker.com | sh
sudo systemctl enable --now docker

# 2. Create the wings directory and data root.
sudo mkdir -p /etc/panasaurus /var/lib/panasaurus/volumes

# 3. Download the binary (or build from source — see §4).
sudo curl -L -o /usr/local/bin/panasaurus-wings \
  https://github.com/panasaurus/wings/releases/download/v0.2.1/panasaurus-wings-linux-amd64
sudo chmod +x /usr/local/bin/panasaurus-wings

# 4. Fetch the node configuration from the panel (see §6 for details).
sudo PANASAURUS_TOKEN='your-panel-api-token' \
  panasaurus-wings --configure \
    --panel-url https://panel.example.com \
    --node-id 1 \
    --token-env PANASAURUS_TOKEN \
    --node-url https://node1.example.com \
    --config /etc/panasaurus/config.yml

# 5. Generate and install a systemd unit.
panasaurus-wings --systemd-unit --config /etc/panasaurus/config.yml \
  | sudo tee /etc/systemd/system/panasaurus-wings.service
sudo systemctl daemon-reload
sudo systemctl enable --now panasaurus-wings

# 6. Verify.
curl -H "Authorization: Bearer $(sudo grep '^token:' /etc/panasaurus/config.yml | awk '{print $2}')" \
  http://127.0.0.1:8081/api/system
```

---

## 3. Install from a Pre-built Binary

Pre-built binaries are published on the [GitHub Releases](https://github.com/panasaurus/wings/releases)
page for each tagged version.

```bash
# Download the Linux x86_64 binary.
sudo curl -L -o /usr/local/bin/panasaurus-wings \
  https://github.com/panasaurus/wings/releases/download/v0.2.1/panasaurus-wings-linux-amd64
sudo chmod +x /usr/local/bin/panasaurus-wings

# Verify it runs.
panasaurus-wings --version
# Panasaurus Wings Rust 0.2.1
```

Other architectures (ARM64, ARMv7) are available on the releases page with
suffixes `-linux-arm64` and `-linux-arm7` respectively.

---

## 4. Build from Source

Building from source is required if you want to customize the code or if
no pre-built binary is available for your platform.

### 4.1 Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version  # should be 1.85 or newer
```

### 4.2 Clone and Build

```bash
git clone https://github.com/panasaurus/wings.git
cd wings
cargo build --release
```

The release binary will be at `target/release/panasaurus-wings`. Copy it
into place:

```bash
sudo cp target/release/panasaurus-wings /usr/local/bin/
sudo chmod +x /usr/local/bin/panasaurus-wings
```

### 4.3 Cross-Compilation

To build for a different target (e.g., ARM64 from an x86_64 host):

```bash
rustup target add aarch64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

The binary will be at
`target/aarch64-unknown-linux-gnu/release/panasaurus-wings`.

### 4.4 Build Notes

- The build uses `aws-lc-rs` for TLS, which requires a C compiler and
  `cmake`. On Debian/Ubuntu: `sudo apt install build-essential cmake pkg-config libssl-dev`.
- The release profile enables LTO and strip for a smaller binary. A clean
  release build takes ~5–10 minutes on a modern machine.
- If you see linker errors about `aws-lc-rs`, ensure `clang` is installed:
  `sudo apt install clang`.

---

## 5. Configuration

Wings reads its configuration from a YAML file (default: `config.yml` in
the working directory; override with `--config /path/to/config.yml`).

### 5.1 Minimal Configuration

The absolute minimum `config.yml`:

```yaml
uuid: 00000000-0000-0000-0000-000000000000
token_id: node-token-id
token: a-very-long-random-string-at-least-32-chars
remote: http://localhost:8080
jwt_audience: http://127.0.0.1:8081
api:
  host: 127.0.0.1
  port: 8081
  upload_limit: 100
  ssl:
    enabled: false
system:
  data: /var/lib/panasaurus/volumes
  default_disk_limit_mb: 20480
  sftp:
    enabled: true
    bind_address: 127.0.0.1
    bind_port: 2022
    max_handles: 64
docker:
  executable: docker
  user: "1000:1000"
  tmpfs_size: 64
  container_pid_limit: 512
  cpu_period: 100000
  installer_limits:
    memory: 1024
    cpu: 100
    timeout_seconds: 3600
allowed_origins:
  - http://localhost:8080
limits:
  jobs: 8
  sftp_connections: 64
  transfer_bytes: 68719476736
recovery:
  enabled: true
  restart_crashed: true
  stop_timeout_seconds: 120
```

### 5.2 Using `--configure` (Recommended)

Instead of hand-writing the config, use the `--configure` command to fetch
it from the panel. This ensures the UUID, token, and panel URL are correct:

```bash
sudo PANASAURUS_TOKEN='ptla_xxxxxxxxxxxxxxxxxxxxxxxx' \
  panasaurus-wings --configure \
    --panel-url https://panel.example.com \
    --node-id 1 \
    --token-env PANASAURUS_TOKEN \
    --node-url https://node1.example.com \
    --config /etc/panasaurus/config.yml
```

The token is read from an environment variable (`--token-env`) so it never
appears in your shell history or process arguments. The command will:

1. Fetch the node configuration from the panel.
2. Set `api.host` to `127.0.0.1` (loopback only — use a reverse proxy for
   remote access).
3. Set `api.ssl.enabled` to `false` (terminate TLS at the proxy).
4. Save the config with `0600` permissions.
5. Refuse to overwrite an existing config (remove it first to reconfigure).

### 5.3 Full Configuration Reference

See [`docs/configuration.md`](docs/configuration.md) for every option,
including the new v0.2.1 sections:

- `system.sftp.shell` — SSH `.wings` CLI settings
- `system.file_history` — file revision history (zstd-compressed)

---

## 6. Register the Node with the Panel

Before Wings can manage servers, the panel must know about the node. This
is done from the panel's admin UI:

1. Go to **Admin → Nodes → Create New**.
2. Fill in the node name, FQDN/URL (the `--node-url` you used), and the
   SFTP port (default 2022).
3. Under **Configuration**, note the **Node UUID** and generate a **Node
   Token**.
4. Run `panasaurus-wings --configure` (see §5.2) on the node, using the
   panel URL, node ID, and an application API token with `nodes > read`
   permission.
5. Back in the panel, assign servers to the node. Wings will discover them
   on next startup.

If the panel and Wings are on the same machine (development setup), the
`remote` URL can be `http://localhost:8080`. For production, always use
HTTPS.

---

## 7. Run as a systemd Service

Wings ships with a `--systemd-unit` command that generates a drop-in
systemd unit file:

```bash
panasaurus-wings --systemd-unit --config /etc/panasaurus/config.yml \
  | sudo tee /etc/systemd/system/panasaurus-wings.service
sudo systemctl daemon-reload
sudo systemctl enable --now panasaurus-wings
```

The generated unit looks like:

```ini
[Unit]
Description=Panasaurus Wings
After=docker.service network-online.target
Wants=network-online.target

[Service]
ExecStart="/usr/local/bin/panasaurus-wings" --config "/etc/panasaurus/config.yml"
Restart=on-failure
RestartSec=5
LimitNOFILE=65536
UMask=0077

[Install]
WantedBy=multi-user.target
```

Check the logs with:

```bash
sudo journalctl -u panasaurus-wings -f
```

---

## 8. Verify the Installation

### 8.1 Check the Process

```bash
sudo systemctl status panasaurus-wings
```

You should see `active (running)` and a log line like:

```
Panasaurus node is ready address=127.0.0.1:8081 servers=0
```

### 8.2 Check the API

```bash
TOKEN=$(sudo grep '^token:' /etc/panasaurus/config.yml | awk '{print $2}')
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8081/api/system | jq .
```

Expected response:

```json
{
  "architecture": "x86_64",
  "cpu_count": 8,
  "kernel_version": "6.1.0-13-amd64",
  "os": "Docker",
  "version": "0.2.1",
  "implementation": "panasaurus-wings-rust",
  "disk_bytes": { "total": 107374182400, "free": 53687091200 }
}
```

### 8.3 Check the OpenAPI Docs (New in v0.2.1)

```bash
curl -s http://127.0.0.1:8081/openapi.json | jq .info
```

### 8.4 Check SFTP

```bash
sftp -P 2022 <panel-username>@127.0.0.1
# Enter your panel password when prompted.
sftp> ls
```

### 8.5 Check the SSH CLI (New in v0.2.1)

```bash
ssh -p 2022 <panel-username>@127.0.0.1 '.wings help'
# Enter your panel password when prompted.
```

### 8.6 Run Diagnostics

```bash
panasaurus-wings --diagnostics --config /etc/panasaurus/config.yml
```

This prints a JSON object with the wings version, OS, architecture, Docker
version, TLS status, SFTP status, and whether the data directory exists.

---

## 9. Upgrading

### 9.1 Manual Upgrade

```bash
# Stop the service.
sudo systemctl stop panasaurus-wings

# Replace the binary.
sudo curl -L -o /usr/local/bin/panasaurus-wings \
  https://github.com/panasaurus/wings/releases/download/v0.2.2/panasaurus-wings-linux-amd64
sudo chmod +x /usr/local/bin/panasaurus-wings

# Start the service.
sudo systemctl start panasaurus-wings
```

### 9.2 Remote Upgrade (New in v0.2.1)

Wings can upgrade itself remotely via the API:

```bash
TOKEN=$(sudo grep '^token:' /etc/panasaurus/config.yml | awk '{print $2}')
curl -X POST -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"url":"https://github.com/panasaurus/wings/releases/download/v0.2.2/panasaurus-wings-linux-amd64","checksum":"<sha256-hex>"}' \
  http://127.0.0.1:8081/api/system/upgrade
```

The upgrade downloads the new binary, verifies the optional sha256
checksum, and atomically swaps it in. **A service restart is required to
activate the new binary** — the panel can trigger this via systemd's
`restart` action, or you can restart manually.

### 9.3 Configuration Migrations

v0.2.1 adds new config sections (`system.sftp.shell`,
`system.file_history`). All new sections have sensible defaults and are
optional — existing v0.2.0 configs work without modification. If you want
to enable file revision history, add:

```yaml
system:
  file_history:
    enabled: true
    zstd_level: 19
    keep_chains: 10
    per_server_disk_budget: 268435456  # 256 MiB
```

---

## 10. Uninstalling

```bash
# Stop and disable the service.
sudo systemctl disable --now panasaurus-wings
sudo rm /etc/systemd/system/panasaurus-wings.service
sudo systemctl daemon-reload

# Remove the binary and config.
sudo rm /usr/local/bin/panasaurus-wings
sudo rm -rf /etc/panasaurus

# Optionally remove all server data (DESTRUCTIVE — only if you're sure).
sudo rm -rf /var/lib/panasaurus/volumes
```

Docker containers created by Wings are labeled
`dev.panasaurus.managed=true` and can be cleaned up with:

```bash
docker ps -a --filter "label=dev.panasaurus.managed=true" -q | xargs -r docker rm -f
```

---

## Next Steps

- [`docs/configuration.md`](docs/configuration.md) — full config reference
- [`docs/sftp.md`](docs/sftp.md) — SFTP + SSH CLI guide
- [`docs/backups.md`](docs/backups.md) — backup configuration
- [`docs/troubleshooting.md`](docs/troubleshooting.md) — common issues

For additional help, join the [Discord](https://discord.gg/tszvW9zKN7).
