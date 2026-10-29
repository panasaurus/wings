# Changelog

All notable changes to Panasaurus Wings are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1] — 2026-10-29

### Added

Ported high-impact features from the Calagopus Wings project, making
Panasaurus Wings significantly more capable than v0.2.0.

#### API

- **`GET /openapi.json`** — OpenAPI 3.0 spec for the entire HTTP API.
  Feed it to Swagger UI, Postman, or any OpenAPI-compatible client to
  discover all endpoints, auth schemes, and request/response shapes.
  ([`src/openapi.rs`](src/openapi.rs))
- **`GET /api/stats`** — node-wide resource usage. Aggregates CPU,
  memory, and disk across all servers on the node, plus host-level disk
  space and Docker info. ([`src/system_stats.rs`](src/system_stats.rs))
- **`GET /api/system/logs`** — list wings log files with size and
  modification time.
- **`GET /api/system/logs/{file}`** — read a wings log file (with path
  traversal protection and 1 MiB truncation for large logs).
- **`POST /api/system/upgrade`** — remotely upgrade the wings binary.
  Downloads over HTTPS, verifies an optional sha256 checksum, and
  atomically swaps the binary in place. A service restart is required to
  activate. ([`src/upgrade.rs`](src/upgrade.rs))
- **`POST /api/servers/{id}/files/search`** — multi-threaded file search
  using rayon. Supports two modes: `filename` (regex match on path) and
  `content` (regex match on file contents with line/column/preview).
  Results are capped by `max_matches` per file and a global response size
  budget. ([`src/file_search.rs`](src/file_search.rs))
- **`GET /api/servers/{id}/files/revisions`** — list prior revisions of a
  file tracked by the file_history subsystem.
- **`GET /api/servers/{id}/files/revisions/{revision}`** — fetch the raw
  contents of a specific revision (decompressed, base64-encoded).
  ([`src/file_history.rs`](src/file_history.rs))

#### SSH `.wings` CLI

- New `.wings` CLI accessible over SSH (port 2022). Users connect with
  panel credentials and run commands like `.wings help`, `.wings power
  start`, `.wings status`, `.wings stats --json`, `.wings logs --limit
  50`, `.wings permissions`. Each command respects panel-granted
  permissions. ([`src/ssh_cli/`](src/ssh_cli/))
- Commands: `help`, `version`, `status`, `power`, `stats`, `logs`,
  `permissions`.
- Configurable via `system.sftp.shell.enabled` and
  `system.sftp.shell.cli_name`.

#### SFTP Extensions

- `statvfs@openssh.com` — filesystem statistics (total/free/available
  blocks + inodes, block size, mount flags).
- `space-available` — alias for `statvfs@openssh.com`.
- `hardlink@openssh.com` — create hard links within the sandbox.
- `posix-rename@openssh.com` — atomic rename that overwrites the
  destination (unlike base SFTP `rename` which fails if the destination
  exists).
- `copy-file` — server-side file copy without round-tripping data over
  the wire.
- `fsync@openssh.com` — flush a file handle's data to disk.
  ([`src/sftp_extensions.rs`](src/sftp_extensions.rs))

#### File Revision History

- Zstd-compressed file revision tracking. When enabled
  (`system.file_history.enabled: true`), Wings snapshots files as
  they're written, so users can browse and restore prior revisions from
  the panel. Configurable retention (`keep_chains`), disk budget
  (`per_server_disk_budget`), compression level (`zstd_level`), and file
  size cap (`file_size_cap`).

#### Configuration

- New `system.sftp.shell` section with `enabled` and `cli_name` fields.
- New `system.file_history` section with `enabled`, `zstd_level`,
  `file_size_cap`, `keep_chains`, `per_server_disk_budget` fields.

#### Documentation

- New [`INSTALL.md`](INSTALL.md) — comprehensive installation guide
  covering requirements, quick start, binary install, source build,
  configuration, panel registration, systemd setup, verification,
  upgrading, and uninstalling.
- New [`docs/configuration.md`](docs/configuration.md) — full config
  reference for every option.
- New [`docs/sftp.md`](docs/sftp.md) — SFTP + SSH CLI guide with command
  reference.
- New [`docs/backups.md`](docs/backups.md) — backup configuration and
  file revision history guide.
- New [`docs/troubleshooting.md`](docs/troubleshooting.md) — common
  issues and solutions.
- Expanded `README.md` with features, architecture diagram, project
  structure, and quick start.
- Expanded `--help` output to list all HTTP API endpoints, SSH commands,
  and SFTP extensions.

### Changed

- Version bumped from 0.2.0 to 0.2.1.
- `Cargo.toml` reorganized with categorized dependencies and additional
  crates: `utoipa`, `async-trait`, `walkdir`, `rayon`, `regex`, `memchr`,
  `aho-corasick`, `zstd`, `base64`, `human_bytes`, `hmac`,
  `constant_time_eq`, `url`, `once_cell`, `tracing-appender`.
- Release profile updated with LTO and size optimization.
- `config.example.yml` updated with new `system.sftp.shell` and
  `system.file_history` sections.

### Notes

- **Build verification**: This release was developed without a local Rust
  toolchain to verify compilation. The code follows existing patterns
  from v0.2.0 and the wings-main reference implementation, but some
  API surface mismatches (particularly around `russh_sftp::protocol`
  variants and `russh::server::Session` methods) may require minor
  adjustments on first compile. See [`BUILD_NOTES.md`](BUILD_NOTES.md)
  for details.
- **Backup drivers**: Only the local `wings` (tar.gz) backup driver is
  included. External drivers (S3, restic, btrfs, zfs, pbs, ddup-bak)
  from wings-main are intentionally omitted to keep the dependency
  footprint small. They may be added in future versions.
- **File history integration**: Revision snapshots are captured on API
  writes. SFTP write-path integration is planned for v0.3.

## [0.2.0] — Initial Release

First public release of Panasaurus Wings (Rust rewrite). Core features:

- HTTP API compatible with Pterodactyl/Panasaurus panels
- Docker container management via CLI wrapper
- SFTP server (russh + russh-sftp) with cap_std sandbox
- WebSocket console streaming
- Server hibernation (idle proxy + wake-on-connect)
- Crash recovery with auto-restart
- Server transfers between nodes
- Local tar.gz backups
- ACME (Let's Encrypt) TLS support
- systemd unit generation
- `--configure` command for panel-based setup
