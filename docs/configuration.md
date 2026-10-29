# Configuration Reference — Panasaurus Wings v0.2.1

Wings reads its configuration from a YAML file (default: `config.yml`;
override with `--config /path/to/config.yml`). The config is validated on
startup — invalid configs are rejected before the daemon starts serving.

This document covers every configuration option, grouped by section.

---

## Top-Level Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `uuid` | string (UUID) | yes | The node's UUID, as registered in the panel. |
| `token_id` | string | yes | The node's token ID (from the panel). |
| `token` | string (≥32 chars) | yes | The node's authentication token. Used for panel→wings and wings→panel auth. |
| `remote` | string (URL) | yes | The panel's base URL. Must be HTTPS or loopback. |
| `jwt_audience` | string (URL) | yes | The expected `aud` claim in JWTs from the panel. |
| `api` | object | yes | HTTP API server settings. |
| `system` | object | yes | Filesystem + SFTP + file history settings. |
| `docker` | object | no | Docker settings (has defaults). |
| `allowed_origins` | list of URLs | no | CORS origins. Should include the panel URL. |
| `hibernation` | list | no | Hibernation routes (see `hibernation.example.yml`). |
| `allowed_mounts` | list of paths | no | Host paths that can be bind-mounted into servers. |
| `downloads` | object | no | Remote download pull settings. |
| `limits` | object | no | Concurrency limits. |
| `recovery` | object | no | Crash recovery settings. |

---

## `api` — HTTP API Server

```yaml
api:
  host: 127.0.0.1
  port: 8081
  upload_limit: 100
  ssl:
    enabled: false
    cert: ""
    key: ""
    acme_domain: null
    acme_email: null
    acme_production: false
```

| Field | Default | Description |
|-------|---------|-------------|
| `host` | — | Bind IP. Must be loopback unless `ssl.enabled` is true. |
| `port` | — | Bind port (typically 8081). |
| `upload_limit` | 100 | Max upload size in MiB (1–10240). |
| `ssl.enabled` | false | Enable TLS. If true, either provide `cert`+`key` or `acme_domain`. |
| `ssl.cert` | "" | Path to the TLS certificate (PEM). |
| `ssl.key` | "" | Path to the TLS private key (PEM). |
| `ssl.acme_domain` | null | Domain for automatic Let's Encrypt certs. |
| `ssl.acme_email` | null | Email for Let's Encrypt notifications. |
| `ssl.acme_production` | false | Use the production Let's Encrypt endpoint (false = staging). |

**Security**: The plaintext API (`ssl.enabled: false`) must bind to a
loopback IP. For remote access, use a TLS-terminating reverse proxy
(nginx, Caddy, Traefik) in front of Wings.

---

## `system` — Filesystem + SFTP + File History

```yaml
system:
  data: /var/lib/panasaurus/volumes
  default_disk_limit_mb: 20480
  sftp:
    enabled: true
    bind_address: 127.0.0.1
    bind_port: 2022
    max_handles: 64
    shell:          # NEW in v0.2.1
      enabled: true
      cli_name: ".wings"
  file_history:    # NEW in v0.2.1
    enabled: false
    zstd_level: 19
    file_size_cap: 1048576
    keep_chains: 10
    per_server_disk_budget: 268435456
```

### `system.data`
**Required.** The root directory for server volumes. Must be an absolute
path shared with Docker (i.e., accessible from both Wings and Docker
containers). Each server gets a subdirectory named by its UUID.

### `system.default_disk_limit_mb`
Default disk limit in MiB for servers that don't specify one. Must be > 0.

### `system.sftp` — SFTP + SSH Server

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | true | Whether to start the SFTP/SSH server. |
| `bind_address` | 127.0.0.1 | Bind IP for SFTP. |
| `bind_port` | 2022 | Bind port for SFTP. |
| `max_handles` | 64 | Max concurrent open file/dir handles per SFTP session (1–512). |

### `system.sftp.shell` — SSH `.wings` CLI (NEW in v0.2.1)

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | true | Whether to allow `.wings` CLI commands over SSH. |
| `cli_name` | ".wings" | The command prefix users type (e.g., `.wings help`). |

When enabled, users can SSH in with panel credentials and run commands like
`.wings power start`, `.wings status`, `.wings stats`. See
[ssh-cli.md](ssh-cli.md) for the full command reference.

### `system.file_history` — File Revision History (NEW in v0.2.1)

When enabled, Wings takes zstd-compressed snapshots of files as they're
written through the API, so users can browse and restore prior revisions.

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | false | Whether to track file revisions. |
| `zstd_level` | 19 | zstd compression level (1–22). Higher = better compression, more CPU. |
| `file_size_cap` | 1048576 (1 MiB) | Files larger than this are not snapshotted. |
| `keep_chains` | 10 | Max revisions to keep per file before pruning oldest. |
| `per_server_disk_budget` | 268435456 (256 MiB) | Soft cap on total disk used by file_history per server. |

Revisions are stored at
`<system.data>/.panasaurus-node/file_history/<server_uuid>/<file_hash>/<timestamp>-<digest>.zst`.

---

## `docker` — Docker Settings

```yaml
docker:
  executable: docker
  user: "1000:1000"
  tmpfs_size: 64
  container_pid_limit: 512
  cpu_period: 100000
  cpu_shares: 0
  memory_overhead_percent: 0
  cpu_burst_percent: 0
  installer_limits:
    memory: 1024
    cpu: 100
    timeout_seconds: 3600
  network:
    name: null
    dns: []
    enable_icc: false
  registries: {}
```

| Field | Default | Description |
|-------|---------|-------------|
| `executable` | "docker" | Path to the Docker CLI. |
| `user` | "1000:1000" | UID:GID for files inside containers. |
| `tmpfs_size` | 64 | tmpfs size in MiB for `/tmp` in containers. |
| `container_pid_limit` | 512 | Max PIDs per container. |
| `cpu_period` | 100000 | CPU CFS period in microseconds (1000–1000000). |
| `cpu_shares` | 0 | CPU shares (0 = use panel-specified limit). |
| `memory_overhead_percent` | 0 | Memory overhead added to container limit (0–100). |
| `cpu_burst_percent` | 0 | Must be 0 (CPU burst is not yet supported). |
| `installer_limits.memory` | 1024 | Installer container memory in MiB. |
| `installer_limits.cpu` | 100 | Installer container CPU in percent. |
| `installer_limits.timeout_seconds` | 3600 | Installer timeout in seconds (1–7200). |
| `network.name` | null | Custom Docker network name. |
| `network.dns` | [] | Custom DNS servers. |
| `network.enable_icc` | false | Enable inter-container communication. |
| `registries` | {} | Private registry credentials (name → {username, password}). |

---

## `limits` — Concurrency Limits

```yaml
limits:
  jobs: 8
  sftp_connections: 64
  transfer_bytes: 68719476736
```

| Field | Default | Description |
|-------|---------|-------------|
| `jobs` | 8 | Max concurrent background jobs (installs, backups, etc.) (1–64). |
| `sftp_connections` | 64 | Max concurrent SFTP connections (1–4096). |
| `transfer_bytes` | 64 GiB | Max bytes for a single server transfer (1–1 TiB). |

---

## `recovery` — Crash Recovery

```yaml
recovery:
  enabled: true
  restart_crashed: true
  detect_clean_exit: false
  crash_cooldown_seconds: 60
  stop_timeout_seconds: 120
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | true | Whether to persist and restore server desired-state across restarts. |
| `restart_crashed` | true | Whether to auto-restart servers that crash. |
| `detect_clean_exit` | false | If true, a clean exit (code 0) counts as a crash. |
| `crash_cooldown_seconds` | 60 | Cooldown between crash-restart attempts. |
| `stop_timeout_seconds` | 120 | Grace period before force-killing a stopping server (1–3600). |

---

## `downloads` — Remote Download Pull

```yaml
downloads:
  allowed_hosts: []
```

| Field | Default | Description |
|-------|---------|-------------|
| `allowed_hosts` | [] | Hostnames that servers can pull remote downloads from. Must be exact host names (no wildcards, no paths). |

---

## `hibernation` — Idle Server Hibernation

See [`hibernation.example.yml`](../hibernation.example.yml) for the format.
Each route proxies a TCP port to an idle server, waking it on connection
and stopping it after a configurable idle timeout.

---

## Environment Overrides

Wings does not currently support environment-variable overrides for config
values. All configuration must come from the YAML file. The `--configure`
command is the recommended way to populate the file from the panel.

---

## Validation

The config is validated on startup. Common validation errors:

- **"Set real node credentials before starting"** — `token` is less than 32
  characters. Generate a real token in the panel.
- **"The plaintext API must bind to a loopback IP"** — `api.host` is not a
  loopback address and `ssl.enabled` is false. Either bind to 127.0.0.1 or
  enable TLS.
- **"system.data must be an absolute path shared with Docker"** —
  `system.data` is relative. Use an absolute path.
- **"Panel credentials require HTTPS or loopback"** — `remote` uses HTTP
  on a non-loopback host. Use HTTPS in production.
